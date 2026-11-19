#!/bin/sh
# Startet D-Bus (braucht warp-svc) und dann ida-speaker. WARP selbst startet ida-speaker, sobald Cloudflare eingerichtet ist.
set -e
mkdir -p /run/dbus /var/lib/cloudflare-warp /data
rm -f /run/dbus/pid
dbus-daemon --system --fork >/dev/null 2>&1 || echo "dbus startet nicht – WARP läuft eventuell trotzdem"
exec /usr/local/bin/ida-speaker "$@"
