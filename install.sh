#!/bin/sh
# Ein Befehl für alles: curl -fsSL https://raw.githubusercontent.com/L8teNever/ida-speaker/main/install.sh | sh
# Installiert Docker (falls nötig), richtet den Autostart ein und startet ida-speaker. Danach: http://<pi>:8080
set -eu
IMAGE="${IMAGE:-ghcr.io/l8tenever/ida-speaker:latest}"
WIFI_COUNTRY="${WIFI_COUNTRY:-DE}"
SUDO=""; [ "$(id -u)" -eq 0 ] || SUDO="sudo"

if ! command -v docker >/dev/null 2>&1; then
  echo "» Docker fehlt – installiere es …"
  curl -fsSL https://get.docker.com | $SUDO sh
fi
# Autostart: Docker startet beim Booten, der Container mit „unless-stopped“ gleich mit
$SUDO systemctl enable --now docker >/dev/null 2>&1 || true

# WLAN nutzbar machen (Raspberry Pi OS sperrt es, solange kein Land gesetzt ist)
if command -v raspi-config >/dev/null 2>&1; then
  $SUDO raspi-config nonint do_wifi_country "$WIFI_COUNTRY" >/dev/null 2>&1 || true
fi
command -v rfkill >/dev/null 2>&1 && $SUDO rfkill unblock wifi 2>/dev/null || true
if ! systemctl is-active --quiet NetworkManager 2>/dev/null; then
  echo "» Hinweis: NetworkManager läuft nicht – WLAN über die Website geht dann nicht (Raspberry Pi OS ab Bookworm hat ihn)."
fi

echo "» Lade $IMAGE …"
$SUDO docker pull "$IMAGE"
$SUDO docker rm -f ida-speaker >/dev/null 2>&1 || true
$SUDO docker run -d --name ida-speaker --restart unless-stopped \
  --device /dev/snd --device /dev/net/tun --cap-add NET_ADMIN \
  --sysctl net.ipv4.conf.all.src_valid_mark=1 \
  -e TZ="${TZ:-Europe/Berlin}" \
  -p 8080:8080 \
  -v ida-speaker:/data -v ida-speaker-warp:/var/lib/cloudflare-warp \
  -v /run/dbus/system_bus_socket:/run/host-dbus/system_bus_socket \
  -v /etc/hostname:/etc/host-hostname:ro \
  "$IMAGE" >/dev/null

ip="$(hostname -I 2>/dev/null | awk '{print $1}')"
echo ""
echo "Fertig. ida-speaker startet ab jetzt bei jedem Booten von selbst."
echo "Einstellungen: http://${ip:-<pi>}:8080  oder  http://$(hostname).local:8080"
echo "Ohne Netz öffnet der Pi nach ~90 s das WLAN „IDA-Speaker-Setup“ (Passwort: idaspeaker) → http://10.42.0.1:8080"
