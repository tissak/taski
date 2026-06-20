#!/usr/bin/env bash
#
# Uninstall the taski launchd autostart agent: stop it and remove the plist.
#
# Leaves the installed binaries (~/.local/bin/taski-*) and config
# (~/.config/taski/) in place — this only removes the autostart. Remove those
# manually if you want a full uninstall.
#
set -euo pipefail

PLIST="${HOME}/Library/LaunchAgents/com.taski.daemon.plist"

if [[ ! -f "${PLIST}" ]]; then
    echo "No plist at ${PLIST}; nothing to uninstall."
    exit 0
fi

echo "==> Unloading launchd agent..."
launchctl unload "${PLIST}" 2>/dev/null || true

echo "==> Removing plist: ${PLIST}"
rm -f "${PLIST}"

cat <<EOF

Done. The taski daemon will no longer autostart.

The binaries (~/.local/bin/taski-*) and config (~/.config/taski/) were left in place.
Remove them manually for a full uninstall.
EOF
