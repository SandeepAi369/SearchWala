# Patched dependencies

Wired in through `[patch.crates-io]` in the root `Cargo.toml`.

## wreq-util 0.2.0

The published 0.2.0 source, unchanged except for one line in `Cargo.toml`:
the `zstd` requirement is raised from `0.13.3` to `0.14.3`.

zstd 0.13 / zstd-safe 7 carry RUSTSEC-2026-0391, -0398 and -0403, and
wreq-util uses zstd to decompress TLS certificates sent by remote servers, so
the vulnerable code is reachable. Upstream's only fix is the 3.0 release
candidate line. Delete this directory and the `[patch.crates-io]` entry once a
stable wreq-util depends on zstd >= 0.14.3.
