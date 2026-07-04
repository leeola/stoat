#!/usr/bin/env bash
#
# Drive a live rust-analyzer session through the stoat TUI and assert the whole
# live chain -- spawn, initialize, diagnostics delivery, clean shutdown -- from
# the success logs and the protocol transcript.
#
# The stoat binary is a raw-mode TUI, so it runs under script(1)'s pty (a
# tool/CI context has no controlling tty). --timeout self-quits the session and
# reaps the language server, so nothing lingers.

set -euo pipefail

readonly TIMEOUT_SECS=30
readonly LOG_FILTER='warn,stoat=info,stoat_bin=info'

repo_root="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
log_dir="${XDG_STATE_HOME:-$HOME/.local/state}/stoat/logs"

fail() {
    echo "verify_live_lsp: FAIL: $1" >&2
    if [ -n "${2:-}" ] && [ -f "$2" ]; then
        echo "--- $2 (stoat::lsp excerpt) ---" >&2
        grep -E 'stoat::lsp|publishDiagnostics' "$2" | tail -20 >&2 || true
    fi
    exit 1
}

command -v rust-analyzer >/dev/null || fail "rust-analyzer not on PATH"
command -v script >/dev/null || fail "script(1) not on PATH"

cargo build -p stoat_bin --manifest-path "$repo_root/Cargo.toml" >&2
stoat_bin="$repo_root/target/debug/stoat"
[ -x "$stoat_bin" ] || fail "stoat binary not built at $stoat_bin"

fixture="$(mktemp -d)"
trap 'rm -rf "$fixture"' EXIT
(CDPATH= cd -- "$fixture" && cargo init --quiet --name verify_live_lsp_fixture)
cat >"$fixture/src/main.rs" <<'RS'
fn main() {
    let _x: u32 = "";
}
RS

mkdir -p "$log_dir"
before_ra="$(pgrep -x rust-analyzer | sort || true)"
before_logs="$(find "$log_dir" -maxdepth 1 -name 'stoat-*.log' -exec basename {} \; | sort || true)"

echo "verify_live_lsp: running a ${TIMEOUT_SECS}s live session..." >&2
(
    CDPATH= cd -- "$fixture"
    script -q /dev/null env STOAT_LOG="$LOG_FILTER" \
        "$stoat_bin" --text-proto-log true --timeout "$TIMEOUT_SECS" src/main.rs
) >/dev/null 2>&1 || true

after_logs="$(find "$log_dir" -maxdepth 1 -name 'stoat-*.log' -exec basename {} \; | sort || true)"
new_log="$(comm -13 <(printf '%s\n' "$before_logs") <(printf '%s\n' "$after_logs") | grep -E '^stoat-[0-9]+\.log$' | tail -1 || true)"
[ -n "$new_log" ] || fail "no new stoat-<pid>.log appeared in $log_dir"

pid="${new_log#stoat-}"
pid="${pid%.log}"
log_file="$log_dir/$new_log"
rx_file="$log_dir/lsp-$pid.rx.jsonl"

grep -q 'language server initialized' "$log_file" \
    || fail "log missing 'language server initialized'" "$log_file"

grep 'diagnostics applied' "$log_file" | grep 'main.rs' | grep -qE 'count=[1-9][0-9]*' \
    || fail "log missing 'diagnostics applied' for main.rs with count >= 1" "$log_file"

[ -f "$rx_file" ] || fail "transcript missing: $rx_file"
grep 'textDocument/publishDiagnostics' "$rx_file" | grep -q '"diagnostics":\[{' \
    || fail "transcript missing a publishDiagnostics frame with a non-empty diagnostics array" "$rx_file"

after_ra="$(pgrep -x rust-analyzer | sort || true)"
[ "$before_ra" = "$after_ra" ] \
    || fail "rust-analyzer process set changed, this run's server may have leaked (before=[${before_ra//$'\n'/,}] after=[${after_ra//$'\n'/,}])"

echo "verify_live_lsp: OK (pid $pid, log $log_file)"
