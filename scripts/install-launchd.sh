#!/usr/bin/env bash
#
# Install taski as a macOS launchd autostart service.
#
#   - Builds release binaries and installs them to ~/.local/bin
#   - Generates ~/Library/LaunchAgents/com.taski.daemon.plist
#   - Loads the agent: starts now, at login (RunAtLoad), and on crash (KeepAlive)
#
# The daemon reads its vault/db from ~/.config/taski/config.toml, so the plist
# carries NO arguments — edit the config file to change them. (Override per-launch
# is unnecessary for an autostarted daemon; the config file is the source of truth.)
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_DIR="${HOME}/.local/bin"
DATA_DIR="${HOME}/.local/share/taski"
PLIST="${HOME}/Library/LaunchAgents/com.taski.daemon.plist"
DAEMON_BIN="${INSTALL_DIR}/taski-daemon"
LABEL="com.taski.daemon"

echo "==> Building release binaries..."
cargo build --release --workspace

if [[ ! -x "${REPO_ROOT}/target/release/taski-daemon" ]]; then
    echo "ERROR: release build did not produce taski-daemon" >&2
    exit 1
fi

echo "==> Installing binaries to ${INSTALL_DIR}"
mkdir -p "${INSTALL_DIR}"
cp -f "${REPO_ROOT}/target/release/taski-daemon" "${DAEMON_BIN}"
cp -f "${REPO_ROOT}/target/release/taski-tui" "${INSTALL_DIR}/taski-tui"
chmod +x "${DAEMON_BIN}" "${INSTALL_DIR}/taski-tui"

# Warn if the install dir isn't on PATH (the TUI is run interactively; the daemon is
# launched by launchd via absolute path so PATH doesn't matter for it).
case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) ;;
    *)
        echo "NOTE: ${INSTALL_DIR} is not on your PATH. Add it (e.g."
        echo "      export PATH=\"${INSTALL_DIR}:\$PATH\" in your shell rc) to run taski-tui directly."
        ;;
esac

echo "==> Ensuring data dir exists: ${DATA_DIR}"
mkdir -p "${DATA_DIR}"

if [[ ! -f "${HOME}/.config/taski/config.toml" ]]; then
    echo "NOTE: no config file at ~/.config/taski/config.toml — the daemon will fail to"
    echo "      start until you create one with at least a 'vault' line. See docs/setup.md."
fi

echo "==> Writing launchd plist: ${PLIST}"
mkdir -p "$(dirname "${PLIST}")"
cat > "${PLIST}" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>${LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>${DAEMON_BIN}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>${DATA_DIR}/daemon.log</string>
    <key>StandardErrorPath</key>
    <string>${DATA_DIR}/daemon.log</string>
</dict>
</plist>
EOF

echo "==> (Re)loading launchd agent..."
# Unload first (ignore failure on a fresh install), then load.
launchctl unload "${PLIST}" 2>/dev/null || true
launchctl load "${PLIST}"

cat <<EOF

Done. The taski daemon will:
  - start now and at login  (RunAtLoad)
  - restart if it crashes   (KeepAlive)
  - log to ${DATA_DIR}/daemon.log
  - read vault/db from ~/.config/taski/config.toml  (edit there to change them)

Run the TUI:      ${INSTALL_DIR}/taski-tui
Tail daemon log:  tail -f ${DATA_DIR}/daemon.log
Stop the daemon:  launchctl unload "${PLIST}"
Uninstall:        ${REPO_ROOT}/scripts/uninstall-launchd.sh
EOF
