#!/bin/sh
# Ein Befehl für alles: curl -fsSL https://raw.githubusercontent.com/L8teNever/ida-speaker/main/install.sh | sh
# Installiert Docker (falls nötig), richtet den Autostart ein und startet ida-speaker. Danach: http://<pi>:8080
# Später nur updaten: ida-speaker-update  (oder: curl … | UPDATE=1 sh) – holt das neue Image und legt den Container neu an
set -eu
IMAGE="${IMAGE:-ghcr.io/l8tenever/ida-speaker:latest}"
WIFI_COUNTRY="${WIFI_COUNTRY:-DE}"
UPDATE="${UPDATE:-0}"
SUDO=""; [ "$(id -u)" -eq 0 ] || SUDO="sudo"

if [ "$UPDATE" = "1" ]; then
  command -v docker >/dev/null 2>&1 || { echo "» Docker fehlt – erst einmal ohne UPDATE=1 installieren."; exit 1; }
  echo "» Suche Update für $IMAGE …"
  $SUDO docker pull -q "$IMAGE" >/dev/null
  new="$($SUDO docker image inspect -f '{{.Id}}' "$IMAGE")"
  cur="$($SUDO docker inspect -f '{{.Image}}' ida-speaker 2>/dev/null || true)"
  if [ "$new" = "$cur" ] && [ "$($SUDO docker inspect -f '{{.State.Running}}' ida-speaker 2>/dev/null)" = "true" ] \
     && [ "$($SUDO docker inspect -f '{{.State.Running}}' ida-speaker-spotify 2>/dev/null)" = "true" ]; then
    echo "Schon aktuell – nichts zu tun."
    exit 0
  fi
  # Seit Spotify: avahi für die Suche im WLAN
  if command -v apt-get >/dev/null 2>&1 && ! command -v avahi-daemon >/dev/null 2>&1; then
    $SUDO apt-get install -y -q avahi-daemon >/dev/null 2>&1 || true
  fi
else

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
# Bluetooth-Lautsprecher: BlueZ und bluez-alsa auf dem Pi (der Container steuert sie über D-Bus)
if command -v apt-get >/dev/null 2>&1; then
  if systemctl is-active --quiet pipewire 2>/dev/null || pgrep -x pipewire >/dev/null 2>&1; then
    echo "» Hinweis: PipeWire läuft (Desktop-System) – Bluetooth-Lautsprecher dann besser mit Raspberry Pi OS Lite."
  else
    echo "» Richte Bluetooth ein (bluez, bluez-alsa) …"
    $SUDO apt-get update -q >/dev/null 2>&1 || true
    $SUDO apt-get install -y -q bluez bluez-alsa-utils >/dev/null 2>&1 || echo "» Bluetooth-Pakete ließen sich nicht installieren – Bluetooth-Lautsprecher gehen dann nicht."
    $SUDO systemctl enable --now bluetooth >/dev/null 2>&1 || true
    $SUDO systemctl enable --now bluealsa >/dev/null 2>&1 || true
    command -v rfkill >/dev/null 2>&1 && $SUDO rfkill unblock bluetooth 2>/dev/null || true
  fi
fi
# Spotify: avahi für die Suche im WLAN (schadet nicht, falls Soloist es nicht braucht)
if command -v apt-get >/dev/null 2>&1 && ! command -v avahi-daemon >/dev/null 2>&1; then
  echo "» Richte avahi ein (Spotify-App findet den Lautsprecher) …"
  $SUDO apt-get install -y -q avahi-daemon >/dev/null 2>&1 || echo "» avahi ließ sich nicht installieren – in der Spotify-App taucht der Lautsprecher dann nicht auf."
fi
command -v systemctl >/dev/null 2>&1 && $SUDO systemctl enable --now avahi-daemon >/dev/null 2>&1 || true
if ! systemctl is-active --quiet NetworkManager 2>/dev/null; then
  echo "» Hinweis: NetworkManager läuft nicht – WLAN über die Website geht dann nicht (Raspberry Pi OS ab Bookworm hat ihn)."
fi

# Kurzer Update-Befehl für später
printf '#!/bin/sh\n# Holt das neueste ida-speaker-Image und startet den Container damit neu (Einstellungen bleiben)\ncurl -fsSL https://raw.githubusercontent.com/L8teNever/ida-speaker/main/install.sh | UPDATE=1 sh\n' \
  | $SUDO tee /usr/local/bin/ida-speaker-update >/dev/null && $SUDO chmod +x /usr/local/bin/ida-speaker-update || true

echo "» Lade $IMAGE …"
$SUDO docker pull "$IMAGE"
fi
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
# Spotify Soloist: zweiter Container im Netzwerk des Pi (die Spotify-App findet ihn nur so im WLAN)
$SUDO docker rm -f ida-speaker-spotify >/dev/null 2>&1 || true
$SUDO docker run -d --name ida-speaker-spotify --restart unless-stopped --network host \
  -v ida-speaker:/data --entrypoint /usr/bin/tini \
  "$IMAGE" -g -- /usr/local/bin/soloist.sh >/dev/null

ip="$(hostname -I 2>/dev/null | awk '{print $1}')"
echo ""
if [ "$UPDATE" = "1" ]; then
  $SUDO docker image prune -f >/dev/null 2>&1 || true
  echo "Update fertig. Website neu laden: http://${ip:-<pi>}:8080"
  exit 0
fi
echo "Fertig. ida-speaker startet ab jetzt bei jedem Booten von selbst."
echo "Update später: ida-speaker-update"
echo "Einstellungen: http://${ip:-<pi>}:8080  oder  http://$(hostname).local:8080"
echo "Ohne Netz öffnet der Pi nach ~90 s das WLAN „IDA-Speaker-Setup“ (Passwort: idaspeaker) → http://10.42.0.1:8080"
