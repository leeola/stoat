#!/usr/bin/env bash
#
# Compile every feature combination in the workspace, so feature-gated code
# cannot rot unseen behind a green default build.
#
# A plain `cargo check --workspace --all-targets` never compiles a gated
# module. `stoat::fixture` exists only under `--features fixture`, and the
# tests that drive it (fixture_live, foreign_terminal) declare
# required-features, so they are skipped rather than reported as uncovered.
# This run covers stoat and stoat_bin over {none, fixture, perf,
# fixture+perf} -- including stoat_bin's cfg(fixture) branches -- and stoatty
# and stoatty_render over {none, perf}. cargo-hack derives the powerset from
# the manifests, so a new feature or a new crate is covered without editing
# this script.
#
# check rather than clippy: the matrix guards compilation, while lint severity
# stays on the default build's `cargo clippy --workspace --all-targets`.
#
# A green run prints one line, because this runs before every commit and its
# output competes with everything else a reviewer reads. A failing run prints
# the whole captured cargo log instead. The first run pays a cold compile of
# every combination; warm runs are incremental.

set -euo pipefail

repo_root="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

fail() {
    echo "check-features: FAIL: $1" >&2
    exit 1
}

command -v cargo-hack >/dev/null \
    || fail "cargo-hack not on PATH. Re-enter the dev shell with 'nix develop', or on a machine outside it run 'cargo install cargo-hack'"

log="$(mktemp)"
trap 'rm -f "$log"' EXIT

if ! cargo hack check --workspace --feature-powerset --all-targets \
    --manifest-path "$repo_root/Cargo.toml" >"$log" 2>&1; then
    cat "$log" >&2
    fail "the feature powerset does not compile"
fi

echo "check-features: OK (workspace feature powerset compiles)"
