# Stoat

## Verification

These four commands are the project's lint and test commands. Run all of them
before every commit.

```sh
cargo fmt --check
cargo clippy --workspace --all-targets
cargo test --workspace
scripts/check-features.sh
```

The first three never compile feature-gated code: `stoat::fixture` exists only
under `--features fixture`, and the tests that drive it declare
required-features, so they are skipped rather than reported as uncovered. A
gated module can therefore break while those three stay green, which is what
`scripts/check-features.sh` exists to catch. It needs `cargo-hack`, which the
flake devshell provides.
