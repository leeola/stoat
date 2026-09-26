#!/usr/bin/env bash
#
# Run the tests behind the `fixture` feature, so a fixture or the live harness
# does not break unseen behind a green default build.
#
# `cargo test --workspace` never builds these tests. `stoat::fixture` exists
# only under `--features fixture`, and the tests that drive it declare
# required-features, so cargo skips them without a report.
# scripts/check-features.sh compiles them but runs none of them. This run
# covers the unit tests inside `stoat::fixture`, the live harness tests
# (fixture_live), the pty test against the real binary (foreign_terminal), and
# the fixture catalog (fixture_catalog).
#
# The lib's unit tests run under the `fixture::` filter, because the workspace
# run already covers every other lib test. The binary builds first, because
# foreign_terminal spawns the `stoat` binary beside its own test executable,
# and a library test run does not build that binary. The LSP tests in
# fixture_live start rust-analyzer and fail without it, so the script checks
# for it before it compiles anything.
#
# A green run prints one line, because this runs before every commit and its
# output competes with everything else a reviewer reads. A failing run prints
# the failing command's captured cargo log instead. The first run compiles
# `stoat` under the feature from cold. Later runs are incremental.

set -euo pipefail

repo_root="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
manifest="$repo_root/Cargo.toml"

fail() {
    echo "check-fixtures: FAIL: $1" >&2
    exit 1
}

command -v rust-analyzer >/dev/null \
    || fail "rust-analyzer not on PATH. Re-enter the dev shell with 'nix develop', or on a machine outside it install rust-analyzer"

log="$(mktemp)"
trap 'rm -f "$log"' EXIT

step() {
    local failure="$1"
    local subcommand="$2"
    shift 2
    if ! cargo "$subcommand" --manifest-path "$manifest" "$@" >"$log" 2>&1; then
        cat "$log" >&2
        fail "$failure"
    fi
}

step "stoat_bin does not build under the fixture feature" \
    build -p stoat_bin --features fixture
step "the stoat::fixture unit tests fail" \
    test -p stoat --features fixture --lib -- fixture::
step "the live fixture tests fail" \
    test -p stoat --features fixture --test fixture_live --test foreign_terminal
step "the fixture catalog tests fail" \
    test -p stoat_bin --features fixture --test fixture_catalog

echo "check-fixtures: OK (the fixture tier passes)"
