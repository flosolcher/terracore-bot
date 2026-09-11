# terracore-bot

An automation daemon for [Terracore](https://www.terracoregame.com/), the idle
exploration game on Hive. Rust, one binary, keys in an encrypted wallet, and
transactions signed offline through [hivecomb](https://crates.io/crates/hivecomb).

It attacks, claims, collects finished missions, fights planet bosses and buys stat
upgrades — for as many accounts as you configure, each with its own settings.

```
$ terracore-bot targets
@youraccount  damage 3436, 8 attacks, 5 claims -- 100 rows on the board, 99 reachable
   1. @mirafun             17703.01 scrap    9349.46 expected  defense  2431  dodge 47.2%
   2. @affliction           9042.89 scrap    7727.27 expected  defense  2591  dodge 14.5%
   3. @c0ff33a              7339.74 scrap    5532.51 expected  defense  2571  dodge 24.6%
  refused: 1 battled in the last minute
```

## Why a rewrite

The old bot was a 2023 Python script against `beem` and the `terracore.herokuapp.com`
API. That endpoint is gone, the game has grown missions, planets, crates and a
marketplace, and `beem` has been unmaintained since 2021 — its default install falls
back to pure-Python ECDSA and it ships the pre-hardfork-24 chain id.

This is a fresh implementation. Nothing was carried over but the idea.

## What it does

| | needs | notes |
|---|---|---|
| **Attack** | posting key | Picks targets with the game client's own rules and ranking. |
| **Claim** | posting key | Empties the stash into your Hive-Engine balance. |
| **Missions** | posting key | Collects finished missions. Never *starts* one — that costs SCRAP. |
| **Boss fights** | **active key** | Burns FLUX per fight, four-hour cooldown per planet. Off by default. |
| **Spending** | **active key** | Turns surplus SCRAP into stats, crit and stake, rotating between them. Off by default. |

The last two move Hive-Engine tokens, so they need an active authority. They are
disabled in the shipped config, and even when enabled they refuse to run unless an
active key is actually in the wallet — you get one warning at startup and a skip line
each cycle, never a silent no-op.

### How targets are chosen

Ported from the game's own client bundle, because the client is the authority on what
the server will accept — attacking someone it would have filtered out spends an attack
for nothing. A target is skipped when:

- its defense is at or above your damage (unless you spend a focus charge),
- it was battled in the last 60 seconds,
- it registered less than 24 hours ago,
- it holds an active protection consumable (24 hours from the most recent charge),
- it has no scrap, or the row is not a real player.

On top of those the bot adds a blacklist, a dodge ceiling, a minimum worth attacking,
and an optional margin on the damage comparison.

Ranking is the client's too: **scrap discounted by the dodge chance**. A dodged attack
still spends the attack, so a target holding 17,000 scrap that dodges half the time is
worth less than one holding 9,000 that never does.

## Install

Rust 1.88 or newer.

```bash
git clone <this repo> && cd terracore-bot
cargo build --release
# target/release/terracore-bot
```

## Set up

```bash
./setup.sh
```

It checks for Rust, builds, writes a `config.toml` for the account you name, creates
the encrypted wallet, and offers to import your keys. It is safe to re-run: anything
that already exists is left alone, because a wallet is not a file a setup script
should have opinions about. Run it without a terminal and it does the parts that need
no input and prints the rest as commands.

Or by hand, which is all the script does:

```bash
cp config.example.toml config.toml
$EDITOR config.toml            # add your account name under [accounts.…]

terracore-bot wallet init      # asks for a passphrase, twice
terracore-bot wallet import --account youraccount --role posting
```

The WIF is read without echo, so it never lands in your shell history. Add an active
key only if you want boss fights or upgrades:

```bash
terracore-bot wallet import --account youraccount --role active
```

Check what you have, then rehearse before you commit to anything:

```bash
./start.sh check               # the settings each account ends up with
./start.sh status              # live state, no passphrase needed, no broadcasts
./start.sh targets             # who it would attack right now, and why not the rest
./start.sh --dry-run once      # a full cycle: signs everything, broadcasts nothing
```

Then run it:

```bash
./start.sh                     # or: terracore-bot run
```

`start.sh` rebuilds first if the binary is missing or older than `src/`, then hands
over with `exec`, so Ctrl-C reaches the bot rather than a wrapper. Anything you pass
goes straight through — `./start.sh status`, `./start.sh --dry-run once`.

Both scripts read `TERRACORE_CONFIG` if you keep the config somewhere other than
`./config.toml`.

Ctrl-C stops after the action in flight rather than halfway through an attack run;
press it twice to quit immediately.

### Unattended

The passphrase comes from `$TERRACORE_WALLET_PASSPHRASE` when it is set, and from a
prompt otherwise. A systemd unit:

```ini
[Service]
Environment=TERRACORE_WALLET_PASSPHRASE=…
ExecStart=/usr/local/bin/terracore-bot --config /etc/terracore-bot/config.toml run
Restart=always
RestartSec=60
```

Prefer `EnvironmentFile=` with a `0600` file over putting the passphrase in the unit.

## Configuring

One TOML file. Everything under `[defaults]` applies to every account; anything under
`[accounts.<name>.<section>]` overrides just that key for just that account. The merge
is a deep merge of the raw tables, so an override only names what it changes:

```toml
[defaults.attack]
max_enemy_dodge = 60.0
delay_secs = 20

[accounts.alice]

[accounts.bob]
[accounts.bob.attack]
max_enemy_dodge = 25.0        # bob is picky; everything else is inherited

[accounts.bob.upgrade]
enabled = true
stats = ["engineering", "damage"]
min_scrap_reserve = 5000.0
```

An unknown key is an error, not a silently ignored setting. `terracore-bot check`
prints the merged result per account.

See [`config.example.toml`](config.example.toml) — every option is documented there.

## What it does with your SCRAP

`[defaults.spend]` decides what surplus SCRAP becomes. It is **not** a waterfall —
each cycle the spendable balance is split between the goals by `weight`, so all of
them advance together, and a goal that has reached its ceiling hands its share back
to the others instead of wasting it.

```
liquid balance after claiming
  │
  ├─ min_scrap_reserve                      never touched
  │
  ├─ stash headroom                         NOT rotated: a full stash stops all
  │                                         attacking, so it is a need
  │
  ├─ engineering  weight 3 ──┐
  ├─ stake        weight 2 ──┤ split by weight, each with its own stop condition
  ├─ favor        weight 1 ──┘
  │
  └─ leftover stays liquid                  so it accumulates into the next purchase
```

The weights are a starting point, not a claim to be optimal — but they come from the
game's own curves rather than taste:

- **Engineering** is the only goal that *compounds*: it raises mining income, which
  pays for everything else. Below the game's 333 softcap a point mines back its cost
  in roughly as many days as your current level, so `max_payback_days` expresses the
  stopping point exactly and it self-limits near the cap.
- **Staking is not spending.** The SCRAP stays yours; it buys dodge, luck and stash
  ceiling for only the cost of the unstaking cooldown. Dodge is cheap to about 15%
  (~130k staked) and then walls hard.
- **Favor** buys crit and is burned for good. Its price is flat inside a band and
  doubles at every edge, so the ceiling is on the *marginal* price rather than a
  target percentage — `max_scrap_per_crit_point` stops at the next cliff wherever it
  happens to be, and a fixed percentage goes stale as the account grows.
- **Damage** is bought only on evidence: the bot already counts how much of the
  battle board is out of reach, and buys only when that exceeds
  `min_unreachable_percent`. **Defense** has no such signal, so it is opt-in.

Every curve in `src/curves.rs` is transcribed from the game client and pinned by
tests, anchored on a real account showing 44,000 favor and 10.933% crit. If the game
changes a formula, those tests fail rather than the bot quietly misspending.

`./start.sh status` prints the marginal price of each goal, so you can see what the
next point of anything would cost before enabling this at all.

## The control panel

Optional, off by default. A small web UI for watching the bot and editing the
settings, served by the bot itself.

```toml
[web]
enabled = true
bind = "127.0.0.1:8787"

[web.access]
youraccount = "admin"
a-friend    = "operator"
```

`./setup.sh` offers to fill that in for you. There is **no separate command**:
`./start.sh` serves the panel alongside the bot and prints the URL on start-up.
`./start.sh check` says whether it is on and who may log in.

### Login is Hive Keychain

No password is stored, and no key is entered:

1. the browser asks for a challenge for `@you`,
2. Keychain signs that exact string with your posting key,
3. the server recovers the key from the signature and checks it against **your
   posting authority as the chain reports it**.

Step 3 is the one that matters. Recovering a key from a signature proves only that
the signature is well formed — a tampered signature simply recovers a *different*
key. It becomes proof of identity only when the recovered key is checked against an
authority fetched from Hive. There is a live test for exactly that:

```bash
cargo test -- --ignored --nocapture
# refused: that key is not in @edsulivan's posting authority
```

### Roles

An account not listed in `[web.access]` cannot log in at all, however good its
signature.

| role | can |
|---|---|
| `admin` | every account's settings, pause / resume / run now, the log |
| `operator` | **only its own** account — cannot see or touch any other |
| `viewer` | reads; changes nothing |

### Keys are not managed here

The panel shows which authorities the wallet holds — account, posting yes/no, active
yes/no — and nothing else. Private keys are never displayed and never accepted over
HTTP. Importing one is a `terracore-bot wallet import` away and does not need to
cross a network, even a loopback one.

### Editing settings

The TOML file stays the source of truth. The panel shows *effective* values, marks
the ones that override `[defaults]` with a dot, and on save rewrites the file in
place — comments, key order and formatting intact. A value dragged back onto the
default has its override deleted rather than restated, so an override present is an
override intended. The bot re-reads the file before its next cycle.

A save is a **replace**, not a merge: the panel sends the whole settings object it
was given. A hand-written `PUT` carrying only some keys will reset the rest to their
compiled-in defaults and drop the account's other overrides.

Everything else about the panel is deliberately small: `SameSite=Strict` HttpOnly
cookie plus a required custom header on every mutation, a content security policy
that permits no external origin except the browser-extension schemes Keychain needs
to publish itself, loopback by default, and a warning in the log if you bind it
anywhere else.

### What is and is not verified

Every route was exercised against a running instance, the role scoping and the config
round-trip have tests over real HTTP, the authority check is tested offline against
synthetic authorities and live against the chain, and all four views were rendered in
a browser.

The one path **not** exercised end to end is the Keychain handshake itself, because
that needs a posting key for a real account. The pieces around it are covered from
both sides — challenge issue and reuse, signature recovery, the authority decision,
session lifetime — but if the browser half misbehaves, the first place to look is the
`Content-Security-Policy` header in `src/web/mod.rs`: Keychain publishes
`window.hive_keychain` by injecting a script element, which that policy governs.

## Design notes

**Signing never touches the network.** A Hive transaction needs the chain id, which is
a compile-time constant, and a recent block reference, which is fetched once per cycle
and cached. `beem` called `get_config` over JSON-RPC on the way to every signature; a
slow node made signing slow, and signing sits inside a deadline.

**Keys are encrypted at rest** — scrypt plus AES-GCM, hivecomb's wallet, `0600` on
disk. `PrivateKey` refuses to render itself through `Debug` or `Display`, so no log
line can leak one by accident. The old bot kept posting keys in a plaintext
`config.txt`.

**State is re-read between attacks**, not counted down locally. Attacks, claims and
the stash all move for reasons this bot did not cause, and a stale count is how you
end up broadcasting attacks you do not have.

**A full stash is emptied first.** The game caps the stash at your staked balance plus
one, and refuses to let you attack once it is full — looted scrap would have nowhere
to go. So the cycle is: empty the stash if it is full, fight, bank what was won, then
spend.

**Nothing is guessed about the game.** The custom_json ids, the payload shapes, the
`tx-hash` nonce rule, the upgrade price curve, the four-hour boss cooldown, the 24-hour
protection window — all read out of the game's own client and, where possible, checked
against the live API.

## Layout

```
setup.sh         first-time setup: config, wallet, keys
start.sh         build if needed, then run

src/
  main.rs        CLI
  config.rs      TOML, defaults + per-account overrides, deep-merged
  api.rs         the Terracore REST API and its shapes
  targeting.rs   who to attack, and why not the rest  (pure, tested)
  curves.rs      the game's stat formulas and what the next point costs (pure, tested)
  hive.rs        custom_json construction, signing, broadcast
  keys.rs        the encrypted wallet
  actions.rs     attack, claim, missions, boss, upgrade
  runner.rs      one cycle over every account, and the loop around it
  state.rs       what the bot publishes and the panel reads
  web/
    mod.rs       the HTTP server and its routes
    auth.rs      the Keychain handshake and sessions
    edit.rs      rewriting the config in place
    ui.html      the panel, embedded in the binary
```

```bash
cargo test        # the rules ported from the client are the part worth testing
```

## Not done yet

- Starting missions (as opposed to collecting them) and opening crates, both of which
  spend SCRAP.
- Using consumables — `fury` for four more attacks, `focus` to reach a target above
  your damage — which the bot understands but never spends.
- The mission model is read from the game client rather than confirmed against live
  data: no account checked had a mission in flight. A shape mismatch means "nothing to
  collect", not a crash.

## Credit

The Hive protocol work is [hivecomb](https://github.com/flosolcher/hivecomb)'s, which
is itself a translation of `beem` by Holger Nahrstaedt and `python-graphenelib` by
Fabian Schuh. Terracore is by [CryptoGnome](https://github.com/CryptoGnome).
