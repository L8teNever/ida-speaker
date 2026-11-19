#!/bin/sh
# Installiert Docker (falls nötig) und startet ida-speaker. Danach: http://<dieses-gerät>:8080
set -eu
IMAGE="${IMAGE:-ghcr.io/l8tenever/ida-speaker:latest}"
if ! command -v docker >/dev/null 2>&1; then
  echo "Docker fehlt – installiere es …"
  curl -fsSL https://get.docker.com | sh
fi
SUDO=""; [ "$(id -u)" -eq 0 ] || SUDO="sudo"
$SUDO docker pull "$IMAGE"
$SUDO docker rm -f ida-speaker >/dev/null 2>&1 || true
$SUDO docker run -d --name ida-speaker --restart unless-stopped \
  --device /dev/snd --device /dev/net/tun --cap-add NET_ADMIN \
  --sysctl net.ipv4.conf.all.src_valid_mark=1 \
  -e TZ="${TZ:-Europe/Berlin}" \
  -p 8080:8080 -v ida-speaker:/data -v ida-speaker-warp:/var/lib/cloudflare-warp \
  "$IMAGE"
ip="$(hostname -I 2>/dev/null | awk '{print $1}')"
echo "Fertig. Einstellungen: http://${ip:-<gerät>}:8080"
