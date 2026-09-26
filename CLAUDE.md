# Stoat

## Verification

These six commands are the project's lint and test commands. Run all of them
before every commit.

```sh
cargo fmt --check
cargo clippy --workspace --all-targets
cargo test --workspace
scripts/check-features.sh
scripts/check-darwin.sh
scripts/check-fixtures.sh
```

The first three never compile feature-gated code: `stoat::fixture` exists only
under `--features fixture`, and the tests that drive it declare
required-features, so they are skipped rather than reported as uncovered. A
gated module can therefore break while those three stay green, which is what
`scripts/check-features.sh` exists to catch. It needs `cargo-hack`, which the
flake devshell provides.

`scripts/check-features.sh` compiles the gated tests but runs none of them.
`scripts/check-fixtures.sh` runs them: the unit tests in `stoat::fixture`, the
live harness tests, the pty test against the real binary, and the fixture
catalog. It needs `rust-analyzer`, which the flake devshell provides, because
the live LSP tests start one. Its first run compiles `stoat` under the feature
from cold.

Every command except `scripts/check-darwin.sh` compiles the host target only.
Code under `cfg(target_os = "macos")` and the darwin `libc` signatures
type-check only under a darwin target. Darwin's `openpty` takes mutable
`termios` and `winsize` pointers where Linux takes const ones, and macOS
declares `TIOCSCTTY` narrower than the ioctl request type.
`scripts/check-darwin.sh` cross-checks that target from Linux. It needs the
`aarch64-apple-darwin` std and zig, which the flake devshell provides.
