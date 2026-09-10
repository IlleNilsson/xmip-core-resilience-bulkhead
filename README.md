# xmip-core-resilience-bulkhead

Bulkhead guard: a fixed number of attempts may be in flight at once, and one more is refused. A technology of [xmip-core-resilience](https://github.com/IlleNilsson/xmip-core-resilience).

## Toolchain

`rust-toolchain.toml` pins the toolchain for the whole estate. Do not change it
here.

## Verification

The included workflow is manual-only and calls the versioned shared workflow at
`IlleNilsson/.github@v1`.
