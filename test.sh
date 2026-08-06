#!/usr/bin/env bash
#
# Run the whole suite the way CI would, in the order that fails fastest.
#
#   ./test.sh            use the current session's display
#   ./test.sh --headless run under Xvfb and a private D-Bus session
#
# Nothing here talks to a llama-server. The wire is recorded fixtures and the
# transport is an injected seam, so the suite is the same with the GPU asleep.
set -euo pipefail
cd "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# GTK_A11Y=none skips the accessibility bus, a common source of CI hangs.
# GSETTINGS_BACKEND=memory keeps tests from touching real user state.
export GTK_A11Y=none
export GSETTINGS_BACKEND=memory
export RUST_BACKTRACE=1

# And this one. Background running holds the application open with no window,
# which is exactly what a scheduled assistant wants and exactly what makes an
# integration test that drives the real application never terminate. A test
# must not be trapped by a setting the user happens to have switched on.
export FAMILIAR_NO_BACKGROUND=1

# And so does this. Memory's usage ledger and the dream's journal default to
# $XDG_DATA_HOME/familiar/, which without an override is the real one — a test
# that opened a vault would write counts into the notes you use every day. The
# tests that care pass their own path, and this is the belt to that pair of
# braces: anything that forgets writes into a directory that goes away.
data_dir="$(mktemp -d)"
trap 'rm -rf "$data_dir"' EXIT
export XDG_DATA_HOME="$data_dir"

# Widget tests will need a display; the model tests do not, and are the bulk.
run=(cargo test --all-targets)
if [[ "${1:-}" == "--headless" ]]; then
  command -v xvfb-run >/dev/null || { echo "install xvfb first" >&2; exit 1; }

  # The private bus activates its own xdg-document-portal, which mounts a FUSE
  # fs at $XDG_RUNTIME_DIR/doc. Inheriting the login session's runtime dir means
  # that mount lands on /run/user/$UID/doc, on top of the real portal's; the real
  # one exits 21 and every flatpak launch fails until it is restarted. Hand the
  # session a throwaway runtime dir so its portals stay inside it.
  runtime_dir="$(mktemp -d)"
  chmod 700 "$runtime_dir"
  trap 'rc=$?; fusermount3 -u "$runtime_dir/doc" 2>/dev/null || :; rm -rf "$runtime_dir" "$data_dir"; exit $rc' EXIT
  export XDG_RUNTIME_DIR="$runtime_dir"

  run=(xvfb-run -a dbus-run-session -- cargo test --all-targets)
fi

echo "==> cargo fmt --check"
cargo fmt --check

echo "==> cargo clippy"
cargo clippy --all-targets -- -D warnings

echo "==> ${run[*]}"
"${run[@]}"

echo
echo "All checks passed."
