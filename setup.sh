#!/usr/bin/env bash
#
# First-time setup: a config file, an encrypted wallet, and the keys in it.
#
# Safe to re-run. Nothing that already exists is overwritten -- a wallet is not a
# file you want a setup script to have opinions about.
#
#   ./setup.sh                 interactive
#   TERRACORE_CONFIG=x ./setup.sh   write the config somewhere else

set -euo pipefail

cd "$(dirname "$0")"

CONFIG="${TERRACORE_CONFIG:-config.toml}"
EXAMPLE="config.example.toml"
BIN="target/release/terracore-bot"

# Colour only when talking to a terminal, so a piped log stays readable.
if [ -t 1 ]; then
    BOLD=$(printf '\033[1m'); DIM=$(printf '\033[2m'); RED=$(printf '\033[31m')
    GREEN=$(printf '\033[32m'); OFF=$(printf '\033[0m')
else
    BOLD=""; DIM=""; RED=""; GREEN=""; OFF=""
fi

say()  { printf '%s\n' "$*"; }
step() { printf '\n%s==>%s %s%s%s\n' "$GREEN" "$OFF" "$BOLD" "$*" "$OFF"; }
note() { printf '%s    %s%s\n' "$DIM" "$*" "$OFF"; }
die()  { printf '%s error:%s %s\n' "$RED" "$OFF" "$*" >&2; exit 1; }

interactive=1
[ -t 0 ] || interactive=0

# ---------------------------------------------------------------------------
step "Checking the toolchain"

command -v cargo >/dev/null 2>&1 || die "cargo not found. Install Rust from https://rustup.rs and re-run."
say "cargo $(cargo --version | awk '{print $2}')"

# ---------------------------------------------------------------------------
step "Building"

if [ -x "$BIN" ]; then
    note "already built; cargo will rebuild only what changed"
fi
cargo build --release
say "built $BIN"

# ---------------------------------------------------------------------------
step "Config"

if [ -f "$CONFIG" ]; then
    say "$CONFIG already exists -- left alone."
else
    [ -f "$EXAMPLE" ] || die "$EXAMPLE is missing; run this from a checkout of the repository."

    account=""
    if [ "$interactive" -eq 1 ]; then
        while [ -z "$account" ]; do
            printf 'Your Hive account name (without the @): '
            read -r account || die "no account given"
            # Lowercased with tr rather than the bash-4 case-conversion parameter
            # expansion, which macOS's bash 3.2 does not have.
            account=$(printf '%s' "$account" | tr '[:upper:]' '[:lower:]' | tr -d '[:space:]@')
            if ! printf '%s' "$account" | grep -qE '^[a-z0-9.-]{3,16}$'; then
                say "  '$account' is not a Hive account name (3-16 of a-z 0-9 . -)"
                account=""
            fi
        done
    else
        account="youraccount"
        note "not a terminal: leaving the placeholder account in place"
    fi

    # A temp file and a move, not `sed -i`: the -i flag takes an argument on BSD
    # sed (macOS) and does not on GNU sed, so the portable form is neither.
    sed "s/youraccount/$account/g" "$EXAMPLE" > "$CONFIG.tmp"
    mv "$CONFIG.tmp" "$CONFIG"
    say "wrote $CONFIG for @$account"
    note "edit it to add more accounts, change the schedule, or turn on the web panel"
fi

# ---------------------------------------------------------------------------
step "Wallet"

# `wallet list` reads the wallet file and needs no passphrase, so it succeeds
# exactly when a wallet exists. Probing that way rather than parsing a path out of
# `check` keeps this script from breaking when that output is reformatted.
wallet_exists=0
if "$BIN" --config "$CONFIG" wallet list >/dev/null 2>&1; then
    wallet_exists=1
fi

if [ "$wallet_exists" -eq 1 ]; then
    say "a wallet already exists -- left alone."
else
    if [ "$interactive" -eq 0 ]; then
        note "not a terminal: skipping. Run './setup.sh' from a terminal, or:"
        note "  $BIN --config $CONFIG wallet init"
    else
        say "Creating an encrypted wallet. The passphrase protects every key in it."
        "$BIN" --config "$CONFIG" wallet init
        wallet_exists=1
    fi
fi

# ---------------------------------------------------------------------------
step "Keys"

if [ "$interactive" -eq 0 ] || [ "$wallet_exists" -eq 0 ]; then
    note "skipping key import; run these when you have a terminal:"
    note "  $BIN --config $CONFIG wallet import --account <name> --role posting"
    note "  $BIN --config $CONFIG wallet import --account <name> --role active   # optional"
else
    held=$("$BIN" --config "$CONFIG" wallet list 2>/dev/null | grep '^@' || true)
    if [ -n "$held" ]; then
        say "The wallet already holds:"
        printf '%s\n' "$held" | sed 's/^/    /'
    fi

    say ""
    say "A ${BOLD}posting${OFF} key is enough to attack, claim and collect missions."
    say "An ${BOLD}active${OFF} key is needed only for boss fights and stat upgrades,"
    say "which spend FLUX and SCRAP. Both are off in the config until you turn them on."
    say ""
    note "The key is read without echo and goes straight into the encrypted wallet."

    for role in posting active; do
        case "$role" in
            active) article="an" ;;
            *)      article="a" ;;
        esac
        printf 'Import %s %s key now? [y/N] ' "$article" "$role"
        read -r answer || answer=""
        case "$answer" in
            [yY]*)
                printf 'Which account? '
                read -r who || die "no account given"
                who=$(printf '%s' "$who" | tr '[:upper:]' '[:lower:]' | tr -d '[:space:]@')
                "$BIN" --config "$CONFIG" wallet import --account "$who" --role "$role"
                ;;
            *)
                note "skipped -- add it later with: $BIN wallet import --account <name> --role $role"
                ;;
        esac
    done
fi

# ---------------------------------------------------------------------------
step "Ready"

say "Settings each account ends up with:"
"$BIN" --config "$CONFIG" check | sed 's/^/    /'

cat <<TEXT

Next, in rough order of caution:

    ./start.sh status       what the bot sees. Reads only, no passphrase.
    ./start.sh targets      who it would attack right now, and why not the rest
    ./start.sh --dry-run once   a full cycle: signs everything, broadcasts nothing
    ./start.sh              run for real

TEXT
