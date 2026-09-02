#!/usr/bin/env bash
#
# Type-check the whole workspace for aarch64-apple-darwin, so macOS-only code
# cannot break unseen behind a green host build.
#
# The host build never compiles a `cfg(target_os = "macos")` branch, and it
# reads the linux libc signatures rather than the darwin ones. Both are ways a
# commit passes every other check here and still fails to build on a Mac.
#
# This never links, so no Apple linker and no macOS SDK are involved. Only the
# crates that build C (tree-sitter, the grammars, libgit2-sys, libz-sys) need a
# compiler, and zig carries the darwin libc headers. cc-rs hands an Apple
# target flags zig rejects, so the wrappers below drop them before forwarding.
#
# The nix evaluation that follows exercises the darwin branch of flake.nix,
# which the linux `nix flake check` never reaches. It catches a Nix-level
# error in that branch, such as a missing attribute, not a shell error inside
# the postInstall it builds.
#
# A green run prints one line, because this runs before every commit and its
# output competes with everything else a reviewer reads. A failing run prints
# the whole captured log instead. Output lands under
# target/aarch64-apple-darwin/, so the host build cache is untouched and the
# first run pays a cold compile.

set -euo pipefail

repo_root="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

fail() {
    echo "check-darwin: FAIL: $1" >&2
    exit 1
}

command -v zig >/dev/null \
    || fail "zig not on PATH. Re-enter the dev shell with 'nix develop', or on a machine outside it install zig"

log="$(mktemp)"
wrappers="$(mktemp -d)"
trap 'rm -f "$log"; rm -rf "$wrappers"' EXIT

# zig rejects the `arm64` architecture spelling cc-rs passes and supplies its
# own SDK headers, so the Apple-target flags are dropped rather than forwarded.
cat >"$wrappers/cc" <<'WRAPPER'
#!/usr/bin/env bash
set -euo pipefail

args=()
has_output=
while [ $# -gt 0 ]; do
    case "$1" in
        -arch | -target | -isysroot)
            shift 2
            ;;
        --target=* | -mmacosx-version-min=*)
            shift
            ;;
        -o)
            has_output=1
            args+=("$1")
            shift
            ;;
        *)
            args+=("$1")
            shift
            ;;
    esac
done

# cc-rs probes each flag with a compile that names no output, which would
# otherwise leave a default a.o in whichever crate directory the build script
# runs from. The scratch path is thrown away with the rest of the wrappers.
if [ -z "$has_output" ]; then
    args+=(-o "$CHECK_DARWIN_SCRATCH/probe.o")
fi

exec zig cc -target aarch64-macos "${args[@]}"
WRAPPER

cat >"$wrappers/ar" <<'WRAPPER'
#!/usr/bin/env bash
set -euo pipefail

exec zig ar "$@"
WRAPPER

chmod +x "$wrappers/cc" "$wrappers/ar"

export CHECK_DARWIN_SCRATCH="$wrappers"
export CC_aarch64_apple_darwin="$wrappers/cc"
export AR_aarch64_apple_darwin="$wrappers/ar"

if ! cargo check --target aarch64-apple-darwin --workspace --all-targets \
    --manifest-path "$repo_root/Cargo.toml" >"$log" 2>&1; then
    cat "$log" >&2
    fail "the workspace does not type-check for aarch64-apple-darwin"
fi

if ! nix eval --raw "$repo_root#packages.aarch64-darwin.stoatty.drvPath" >>"$log" 2>&1; then
    cat "$log" >&2
    fail "the darwin stoatty derivation does not evaluate"
fi

echo "check-darwin: OK (workspace type-checks for aarch64-apple-darwin)"
