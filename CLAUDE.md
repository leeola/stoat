# Stoat

## Verification

These five commands are the project's lint and test commands. Run all of them
before every commit.

```sh
cargo fmt --check
cargo clippy --workspace --all-targets
cargo test --workspace
scripts/check-features.sh
scripts/check-darwin.sh
```

The first three never compile feature-gated code: `stoat::fixture` exists only
under `--features fixture`, and the tests that drive it declare
required-features, so they are skipped rather than reported as uncovered. A
gated module can therefore break while those three stay green, which is what
`scripts/check-features.sh` exists to catch. It needs `cargo-hack`, which the
flake devshell provides.

The first four commands compile the host target only. Code under
`cfg(target_os = "macos")` and the darwin `libc` signatures type-check only
under a darwin target. Darwin's `openpty` takes mutable `termios` and
`winsize` pointers where Linux takes const ones, and macOS declares
`TIOCSCTTY` narrower than the ioctl request type. `scripts/check-darwin.sh`
cross-checks that target from Linux. It needs the `aarch64-apple-darwin` std
and zig, which the flake devshell provides.
