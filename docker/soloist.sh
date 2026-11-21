#!/bin/bash
# Zweiter Container ida-speaker-spotify (Netzwerk des Pi, damit die Spotify-App ihn im WLAN findet).
# Lädt Spotify Soloist selbst bei Spotify herunter (darf nicht ins Image), startet es mit Name und Schlüssel aus
# /data/soloist/run.env (schreibt ida-speaker) und reicht die WebSocket-API als /data/soloist/ws.sock durch.
# Ton: in die virtuelle Soundkarte von ida-speaker (PulseAudio unter /data/pulse/native).
D=/data/soloist
BIN=$D/bin/soloist
PORT=${SOLOIST_WS_PORT:-47891}
mkdir -p "$D/bin" "$D/state" /tmp/soloist-run
export PULSE_SERVER=unix:/data/pulse/native XDG_RUNTIME_DIR=/tmp/soloist-run
rm -f "$D/state/soloist.pid"  # neuer Container: alte PID kann zufällig wieder leben
LOG=$D/soloist.log

say() { echo "$*" > "$D/status"; echo "soloist: $*"; }

arch() {
  case "$(uname -m)" in
    aarch64|arm64) echo arm64 ;;
    armv7l|armv6l) echo arm32 ;;
    x86_64) echo x86_64 ;;
  esac
}

fetch() {
  local a tmp f
  a=$(arch)
  [ -n "$a" ] || { say "Soloist gibt es für $(uname -m) nicht"; return 1; }
  say "Lade Spotify Soloist …"
  tmp=$(mktemp -d)
  if curl -fsSL -o "$tmp/s.tar.gz" "https://soloist-builds.spotifycdn.com/soloist_release_$a.tar.gz" \
     && tar -xzf "$tmp/s.tar.gz" -C "$tmp" \
     && f=$(find "$tmp" -type f -name soloist | head -1) && [ -n "$f" ]; then
    install -m 755 "$f" "$BIN.new" && mv "$BIN.new" "$BIN"
    rm -rf "$tmp"
    : > "$D/status"
    "$BIN" --version || true
    missing=$(ldd "$BIN" 2>/dev/null | grep "not found")
    [ -n "$missing" ] && say "Soloist fehlt eine Bibliothek: $missing"
    return 0
  fi
  rm -rf "$tmp"
  say "Download von Spotify Soloist fehlgeschlagen"
  return 1
}

# WebSocket-API nur lokal, ida-speaker kommt über den Socket im geteilten Volume dran
socat "UNIX-LISTEN:$D/ws.sock,fork,unlink-early,mode=600" "TCP:127.0.0.1:$PORT" &

while true; do
  SOLOIST_NAME=""; SOLOIST_KEY=""
  # shellcheck disable=SC1091
  [ -f "$D/run.env" ] && . "$D/run.env"
  if [ -z "$SOLOIST_KEY" ]; then : > "$D/status"; sleep 5; continue; fi
  # Builds laufen nach 90 Tagen ab: nach 60 Tagen neu holen
  if [ ! -x "$BIN" ] || [ -n "$(find "$BIN" -mtime +60)" ]; then
    fetch || { sleep 60; continue; }
  fi
  if [ ! -S /data/pulse/native ]; then say "Warte auf die Musik-Ausgabe von ida-speaker …"; sleep 5; continue; fi
  sum=$(md5sum < "$D/run.env")
  pulse=$(stat -c %i /data/pulse/native 2>/dev/null)
  [ -f "$LOG" ] && [ "$(stat -c %s "$LOG")" -gt 1000000 ] && mv -f "$LOG" "$LOG.1"
  "$BIN" -n "$SOLOIST_NAME" -k "$SOLOIST_KEY" -D "$D/state" -C /tmp/soloist-cache -z 200 -w "127.0.0.1:$PORT" >>"$LOG" 2>&1 &
  pid=$!
  # Name/Schlüssel geändert (Website) oder ida-speaker neu gestartet (neue Musik-Ausgabe): Soloist neu starten
  while kill -0 "$pid" 2>/dev/null; do
    sleep 3
    if [ "$(md5sum < "$D/run.env" 2>/dev/null)" != "$sum" ] || [ "$(stat -c %i /data/pulse/native 2>/dev/null)" != "$pulse" ]; then
      kill "$pid"
    fi
  done
  wait "$pid"; code=$?
  case $code in
    10) say "Spotify Soloist ist abgelaufen – lade die neue Version"; rm -f "$BIN" ;;
    0|143) ;;
    *) say "Spotify Soloist hat sich beendet (Code $code): $(tail -n 3 "$LOG" | tr '\n' ' ' | cut -c1-300)"; sleep 10 ;;
  esac
done
