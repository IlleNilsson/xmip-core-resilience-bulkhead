#![forbid(unsafe_code)]

//! The bulkhead guard — a technology of `xmip-core-resilience` (ADR-0048).
//!
//! A fixed number of attempts may be in flight at once; one more is refused
//! rather than queued, so a slow far end fills its own compartment and no
//! other. The guard takes a slot before an attempt and gives it back after,
//! with an atomic count, so one bulkhead may stand in front of many callers.
//!
//! Order matters. The platform asks the guards in the order given and stops at
//! the first that does not proceed, before and after alike; a slot this guard
//! took is released only when its `after` is reached. Put the bulkhead first,
//! and a guard behind it that waits, refuses or falls back never strands a
//! slot.

use std::sync::atomic::{AtomicUsize, Ordering};

use resilience::{Attempt, Decision, Guard};

/// The bulkhead guard: this many attempts in flight at once.
#[derive(Debug)]
pub struct Bulkhead {
    max_concurrent: usize,
    in_flight: AtomicUsize,
}

impl Bulkhead {
    /// Up to `max_concurrent` attempts at once. Zero is taken as one.
    #[must_use]
    pub const fn new(max_concurrent: usize) -> Self {
        Self {
            max_concurrent: if max_concurrent == 0 {
                1
            } else {
                max_concurrent
            },
            in_flight: AtomicUsize::new(0),
        }
    }

    /// How many attempts may be in flight at once.
    #[must_use]
    pub const fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }

    /// How many attempts hold a slot right now.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::SeqCst)
    }
}

impl Guard for Bulkhead {
    fn technology(&self) -> &'static str {
        "bulkhead"
    }

    fn before(&self, _: u32) -> Decision {
        let taken = self
            .in_flight
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                (current < self.max_concurrent).then_some(current + 1)
            });
        match taken {
            Ok(_) => Decision::Proceed,
            Err(current) => Decision::Refuse(format!(
                "{current} in flight, limit {}",
                self.max_concurrent
            )),
        }
    }

    fn after(&self, _: &Attempt) -> Decision {
        let _ = self
            .in_flight
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                current.checked_sub(1)
            });
        Decision::Proceed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use resilience::{Failure, Guarded, execute};
    use std::cell::Cell;
    use std::time::Duration;

    fn failed() -> Attempt {
        Attempt {
            number: 1,
            elapsed: Duration::ZERO,
            failure: Some(Failure::retryable("again")),
        }
    }

    #[test]
    fn slots_are_taken_before_an_attempt_and_one_past_the_limit_is_refused() {
        let bulkhead = Bulkhead::new(2);
        assert_eq!(bulkhead.technology(), "bulkhead");
        assert_eq!(bulkhead.max_concurrent(), 2);
        assert_eq!(bulkhead.before(1), Decision::Proceed);
        assert_eq!(bulkhead.before(1), Decision::Proceed);
        assert_eq!(bulkhead.in_flight(), 2);
        assert_eq!(
            bulkhead.before(1),
            Decision::Refuse("2 in flight, limit 2".into())
        );
        assert_eq!(bulkhead.in_flight(), 2, "a refusal takes no slot");
    }

    #[test]
    fn a_slot_is_given_back_after_the_attempt_whatever_its_outcome() {
        let bulkhead = Bulkhead::new(1);
        assert_eq!(bulkhead.before(1), Decision::Proceed);
        assert_eq!(bulkhead.after(&failed()), Decision::Proceed);
        assert_eq!(bulkhead.in_flight(), 0);
        assert_eq!(bulkhead.before(1), Decision::Proceed, "free again");
        let succeeded = Attempt {
            number: 1,
            elapsed: Duration::ZERO,
            failure: None,
        };
        assert_eq!(bulkhead.after(&succeeded), Decision::Proceed);
        assert_eq!(bulkhead.after(&succeeded), Decision::Proceed);
        assert_eq!(bulkhead.in_flight(), 0, "never below zero");
        assert_eq!(Bulkhead::new(0).max_concurrent(), 1);
    }

    #[test]
    fn many_threads_never_hold_more_slots_than_the_limit() {
        let bulkhead = Bulkhead::new(3);
        let peak = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..50 {
                        if bulkhead.before(1) == Decision::Proceed {
                            peak.fetch_max(bulkhead.in_flight(), Ordering::SeqCst);
                            std::thread::yield_now();
                            bulkhead.after(&failed());
                        }
                    }
                });
            }
        });
        assert!(peak.load(Ordering::SeqCst) <= 3);
        assert_eq!(bulkhead.in_flight(), 0);
    }

    #[test]
    fn under_execute_the_slot_is_held_for_the_attempt_and_released_after() {
        let bulkhead = Bulkhead::new(1);
        let guards: [&dyn Guard; 1] = [&bulkhead];
        let calls = Cell::new(0);
        let outcome = execute(&guards, || {
            calls.set(calls.get() + 1);
            assert_eq!(bulkhead.in_flight(), 1, "held while the operation runs");
            Ok("done")
        });
        assert_eq!(outcome, Ok(Guarded::Done("done")));
        assert_eq!(calls.get(), 1);
        assert_eq!(bulkhead.in_flight(), 0);

        assert_eq!(
            bulkhead.before(1),
            Decision::Proceed,
            "someone else's attempt"
        );
        let refused = execute(&guards, || {
            calls.set(calls.get() + 1);
            Ok("done")
        });
        assert_eq!(refused, Ok(Guarded::Refused("1 in flight, limit 1".into())));
        assert_eq!(calls.get(), 1, "refused before any attempt");
    }

    /// Tries again on a retryable failure, as the retry technology does.
    struct Again(u32);

    impl Guard for Again {
        fn technology(&self) -> &'static str {
            "retry"
        }

        fn before(&self, _: u32) -> Decision {
            Decision::Proceed
        }

        fn after(&self, attempt: &Attempt) -> Decision {
            match &attempt.failure {
                Some(failure) if failure.is_retryable() && attempt.number < self.0 => {
                    Decision::Wait(Duration::ZERO)
                }
                _ => Decision::Proceed,
            }
        }
    }

    #[test]
    fn a_bulkhead_ahead_of_retry_takes_and_releases_a_slot_per_attempt() {
        let bulkhead = Bulkhead::new(1);
        let guards: [&dyn Guard; 2] = [&bulkhead, &Again(3)];
        let calls = Cell::new(0);
        let outcome: Result<Guarded<()>, Failure> = execute(&guards, || {
            calls.set(calls.get() + 1);
            assert_eq!(bulkhead.in_flight(), 1);
            Err(Failure::retryable("again"))
        });
        assert_eq!(outcome, Err(Failure::retryable("again")));
        assert_eq!(calls.get(), 3);
        assert_eq!(bulkhead.in_flight(), 0, "every retry gave its slot back");
    }
}
