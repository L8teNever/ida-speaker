#!/bin/sh
# Startet D-Bus (braucht warp-svc) und dann ida-speaker. WARP selbst startet ida-speaker, sobald Cloudflare eingerichtet ist.
set -e
mkdir -p /run/dbus /var/lib/cloudflare-warp /data
rm -f /run/dbus/pid
dbus-daemon --system --fork >/dev/null 2>&1 || echo "dbus startet nicht – WARP läuft eventuell trotzdem"
# ida-speaker selbst spricht mit dem D-Bus des Pi (Bluetooth-Audio über bluez-alsa), WARP mit dem eigenen
[ -S /run/host-dbus/system_bus_socket ] && export DBUS_SYSTEM_BUS_ADDRESS=unix:path=/run/host-dbus/system_bus_socket
exec /usr/local/bin/ida-speaker "$@"
