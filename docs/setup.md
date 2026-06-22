# Setup — Taski daily driver

Daily-driver setup for macOS: build the release binaries, point taski at your vault, and autostart the daemon via launchd.

## 1. Build

```sh
cargo build --release --workspace
```

Produces `target/release/taski` (the unified binary) plus the standalone `target/release/taski-daemon` and `target/release/taski-tui`.

## 2. Configure

Easiest — let the daemon write one for you:

```sh
./target/release/taski-daemon --init-config --vault /Users/tissak/obsidian/Personal-PARA
```

This writes `~/.config/taski/config.toml` (refusing to overwrite an existing one) with
your vault and the conventional db path baked in. Drop `--vault` to write a template
with a placeholder to fill in.

Or write it by hand (`~/.config/taski/config.toml`; both fields are optional):

```toml
# Example ~/.config/taski/config.toml
vault = "/Users/tissak/obsidian/Personal-PARA"
db    = "/Users/tissak/.local/share/taski/taski.db"
```

- `vault` — your Obsidian vault root. Required by the daemon (there is no default).
- `db` — SQLite index path. Defaults to `./taski.db` if unset.

Override either per-invocation with `--vault` / `--db` (the CLI flag wins over the config file).

Point taski at an alternate config file with the `TASKI_CONFIG` environment variable. A missing config file is fine (taski falls back to CLI flags / defaults); only a malformed file is an error.

For every option (inbox path, exclude dirs, Obsidian deep-link settings), the TUI color theme — including a ready-made Nord palette — and per-panel layout knobs, see the full **[configuration guide](./config.md)**.

## 3. Autostart the daemon (launchd)

```sh
scripts/install-launchd.sh
```

This:

- installs `taski` (plus the standalone `taski-daemon` / `taski-tui` for backcompat) to `~/.local/bin/`,
- generates `~/Library/LaunchAgents/com.taski.daemon.plist` invoking `taski daemon` — **the plist passes only the `daemon` subcommand** (no vault/db args), because the daemon reads those from your `~/.config/taski/config.toml`,
- starts the daemon now and at login (`RunAtLoad`), keeping it alive across crashes (`KeepAlive`).

Logs stream to `~/.local/share/taski/daemon.log` — `tail -f` it to watch.

## 4. Run the app

```sh
taski                 # daemon + TUI together; drains pending actions and exits on quit
```

That's the daily driver: it runs the daemon (background thread) + TUI (main thread)
together. If launchd's daemon is already running (lock held), `taski` **attaches** — it
runs the TUI only against that daemon and prints
`taski: attached to running daemon (PID X). TUI only — the daemon keeps running after you quit.`

TUI only (a reader; safe alongside any running daemon):

```sh
taski tui
```

## Uninstall

```sh
scripts/uninstall-launchd.sh   # stop the daemon + remove the launchd agent
```

The binaries (`taski`, `taski-daemon`, `taski-tui` under `~/.local/bin/`) and config (`~/.config/taski/`) are left in place — remove them manually if you want a full uninstall.
