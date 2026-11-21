#!/bin/sh
# Startet D-Bus (braucht warp-svc) und dann ida-speaker. WARP selbst startet ida-speaker, sobald Cloudflare eingerichtet ist.
set -e
mkdir -p /run/dbus /var/lib/cloudflare-warp /data
rm -f /run/dbus/pid
dbus-daemon --system --fork >/dev/null 2>&1 || echo "dbus startet nicht – WARP läuft eventuell trotzdem"
# Spotify: virtuelle Soundkarte ida_music, in die Soloist (Container ida-speaker-spotify) spielt; ida-speaker liest sie mit.
# -n: keine Standard-Module, PulseAudio fasst die echte Soundkarte nicht an.
mkdir -p /data/pulse /tmp/pulse-run
rm -f /data/pulse/native
XDG_RUNTIME_DIR=/tmp/pulse-run HOME=/tmp/pulse-run pulseaudio -n --daemonize=yes --exit-idle-time=-1 --use-pid-file=no \
  -L "module-null-sink sink_name=ida_music rate=48000 channels=2" \
  -L "module-native-protocol-unix socket=/data/pulse/native auth-anonymous=1" >/dev/null 2>&1 \
  || echo "PulseAudio startet nicht – Spotify geht dann nicht"
# ida-speaker selbst spricht mit dem D-Bus des Pi (Bluetooth-Audio über bluez-alsa), WARP mit dem eigenen
[ -S /run/host-dbus/system_bus_socket ] && export DBUS_SYSTEM_BUS_ADDRESS=unix:path=/run/host-dbus/system_bus_socket
exec /usr/local/bin/ida-speaker "$@"
