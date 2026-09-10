#!/usr/bin/env bash
#
# Run the bot locally, building first if anything has changed.
#
#   ./start.sh                  run cycles forever
#   ./start.sh status           anything the binary accepts is passed straight through
#   ./start.sh --dry-run once
#
# TERRACORE_CONFIG overrides the config path.

set -euo pipefail

cd "$(dirname "$0")"

CONFIG="${TERRACORE_CONFIG:-config.toml}"
BIN="target/release/terracore-bot"

if [ -t 1 ]; then
    DIM=$(printf '\033[2m'); RED=$(printf '\033[31m'); OFF=$(printf '\033[0m')
else
    DIM=""; RED=""; OFF=""
fi

note() { printf '%s%s%s\n' "$DIM" "$*" "$OFF" >&2; }
die()  { printf '%s error:%s %s\n' "$RED" "$OFF" "$*" >&2; exit 1; }

if [ ! -f "$CONFIG" ]; then
    die "no $CONFIG. Run ./setup.sh first."
fi

# Rebuild when the binary is missing, or when anything it is built from is newer
# than it. `find -newer ... -print -quit` stops at the first hit and behaves the
# same on GNU and BSD find, so this works on macOS too.
needs_build=0
if [ ! -x "$BIN" ]; then
    needs_build=1
    note "no binary yet; building"
elif [ -n "$(find src Cargo.toml Cargo.lock -newer "$BIN" -print -quit 2>/dev/null)" ]; then
    needs_build=1
    note "sources are newer than the binary; rebuilding"
fi

if [ "$needs_build" -eq 1 ]; then
    command -v cargo >/dev/null 2>&1 || die "cargo not found. Install Rust from https://rustup.rs"
    cargo build --release
fi

# The wallet passphrase comes from the environment when it is set and from a
# prompt otherwise, so an unattended run needs the variable and an attended one
# does not. Never echoed, never stored here.
if [ -z "${TERRACORE_WALLET_PASSPHRASE:-}" ] && [ ! -t 0 ]; then
    note "no terminal and no \$TERRACORE_WALLET_PASSPHRASE: the bot will not be able to unlock the wallet"
fi

# `exec` so signals reach the bot directly: Ctrl-C should stop the current action
# cleanly rather than killing a wrapper and orphaning it.
if [ $# -eq 0 ]; then
    exec "$BIN" --config "$CONFIG" run
else
    exec "$BIN" --config "$CONFIG" "$@"
fi
