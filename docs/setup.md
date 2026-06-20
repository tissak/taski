# Setup — Taski daily driver

Daily-driver setup for macOS: build the release binaries, point taski at your vault, and autostart the daemon via launchd.

## 1. Build

```sh
cargo build --release --workspace
```

Produces `target/release/taski-daemon` and `target/release/taski-tui`.

## 2. Configure

Write `~/.config/taski/config.toml` (XDG-style location; both fields are optional):

```toml
# Example ~/.config/taski/config.toml
vault = "/Users/tissak/obsidian/Personal-PARA"
db    = "/Users/tissak/.local/share/taski/taski.db"
```

- `vault` — your Obsidian vault root. Required by the daemon (there is no default).
- `db` — SQLite index path. Defaults to `./taski.db` if unset.

Override either per-invocation with `--vault` / `--db` (the CLI flag wins over the config file).

Point taski at an alternate config file with the `TASKI_CONFIG` environment variable. A missing config file is fine (taski falls back to CLI flags / defaults); only a malformed file is an error.

## 3. Autostart the daemon (launchd)

```sh
scripts/install-launchd.sh
```

This:

- installs both binaries to `~/.local/bin/`,
- generates `~/Library/LaunchAgents/com.taski.daemon.plist` — **the plist carries no arguments**, because the daemon reads vault/db from your `~/.config/taski/config.toml`,
- starts the daemon now and at login (`RunAtLoad`), keeping it alive across crashes (`KeepAlive`).

Logs stream to `~/.local/share/taski/daemon.log` — `tail -f` it to watch.

## 4. Run the TUI

```sh
~/.local/bin/taski-tui
```

Or alias it once: `alias taski="$HOME/.local/bin/taski-tui"`.

## Uninstall

```sh
scripts/uninstall-launchd.sh   # stop the daemon + remove the launchd agent
```

The binaries (`~/.local/bin/taski-*`) and config (`~/.config/taski/`) are left in place — remove them manually if you want a full uninstall.
