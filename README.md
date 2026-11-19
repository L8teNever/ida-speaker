# ida-speaker

Ein Sprach-Lautsprecher für [IDA](https://github.com/L8teNever/IDA-PA) auf dem Raspberry Pi, komplett in Rust.

Er hört lokal und offline auf ein Wake-Word (z. B. „Stella“). Sobald es fällt, verbindet er sich mit dem Voice-Dienst von IDA, und du redest direkt mit ihr: Fragen, Termine, Smart Home, alles, was das Live-Gespräch in IDA kann. Ein Echo-Filter rechnet ihre eigene Stimme aus dem Mikrofon heraus. Sie hört sich also nicht selbst, und du kannst sie unterbrechen.

Alles steckt in **einem Docker-Image**: das Programm, das deutsche Sprachmodell und der Cloudflare-WARP-Client. Eingestellt wird alles über eine Website auf dem Pi.

```
Pi (ein Container)
 ├─ Mikrofon ─► Echo-Filter (WebRTC AEC3) ─► Wake-Word (Vosk, offline)
 │                                              └─ erkannt ─► Ticket ─► WebSocket /live?mode=speaker
 ├─ Lautsprecher ◄─ Stellas Stimme (24 kHz) ──────────────────────────────┘
 ├─ Website :8080 (Einrichtung, Status, Tests)
 └─ Cloudflare WARP ─► Zero Trust ─► privates Netz ─► IDA-Voice-Dienst
```

## Was du brauchst

- Raspberry Pi 4 oder 5 mit 64-Bit-Raspberry-Pi-OS (oder ein anderes arm64-/amd64-Linux)
- USB-Mikrofon und Lautsprecher, oder eine USB-Freisprecheinrichtung. Der Echo-Filter läuft in Software, deshalb reicht einfache Hardware. Stell Mikrofon und Lautsprecher am besten nicht direkt nebeneinander.
- Docker
- IDA mit eingerichtetem Voice-Dienst (`VOICE_URL`, `VOICE_SECRET`)

## Installieren

```sh
curl -fsSL https://get.docker.com | sh       # falls Docker noch fehlt
docker run -d --name ida-speaker --restart unless-stopped \
  --device /dev/snd --device /dev/net/tun --cap-add NET_ADMIN \
  --sysctl net.ipv4.conf.all.src_valid_mark=1 \
  -p 8080:8080 -v ida-speaker:/data -v ida-speaker-warp:/var/lib/cloudflare-warp \
  ghcr.io/l8tenever/ida-speaker:latest
```

Statt `docker run` geht auch die [`docker-compose.yml`](docker-compose.yml) aus diesem Repo (`docker compose up -d`) oder [`install.sh`](install.sh), das beides erledigt.

Dann `http://<pi>:8080` öffnen. Ein Assistent führt durch die Einrichtung:

1. **Passwort** für die Seite festlegen.
2. **IDA:** Adresse des Voice-Dienstes, Gerätename und Schlüssel eintragen, dann „Verbindung testen“ ([so geht's](docs/IDA.md)).
3. **Cloudflare** (optional): Team-Name und Service-Token eintragen. Der Pi kommt dann ins Zero-Trust-Netz ([so geht's](docs/CLOUDFLARE.md)).
4. **Audio:** Mikrofon und Lautsprecher wählen, Testton abspielen, Pegel prüfen.
5. **Wake-Word:** Wort eintragen und Empfindlichkeit einstellen.

Alle Änderungen gelten sofort, ein Neustart ist nicht nötig. Die Einstellungen liegen nur auf dem Pi (`/data/config.toml` im Volume), nie im Repo.

**Update:** `docker pull ghcr.io/l8tenever/ida-speaker:latest`, danach den Container neu erstellen (mit Compose: `docker compose pull && docker compose up -d`). Die Einstellungen bleiben erhalten.

## Benutzen

- „Stella“ sagen (oder dein Wort). Ein kurzer Ton zeigt, dass sie zuhört, und sie meldet sich.
- Einfach reden. Du kannst sie jederzeit unterbrechen.
- Das Gespräch endet, wenn du dich verabschiedest, nach einer einstellbaren Zeit Stille oder über „Auflegen“ auf der Website. Ein zweiter Ton zeigt das Ende.
- Auf der Website kannst du ein Gespräch auch per Knopf starten oder das Wake-Word stummschalten.

## Einstellungen

| Bereich | Was |
|---|---|
| Wake-Word | Wort oder Phrase, Empfindlichkeit, an/aus, „Zuletzt gehört“ zum Testen |
| Audio | Geräte, Lautstärke, Mikrofon-Verstärkung, Signaltöne, Testton, Pegelanzeige |
| Echo-Filter | Echo-Unterdrückung, Rauschunterdrückung, automatische Lautstärke, Notfall-Modus (Mikrofon stumm, solange Stella spricht) |
| Gespräch | Auflegen nach Stille, Höchstdauer, Sprach-Schwelle |
| IDA | Adresse, Gerätename, Schlüssel, Verbindungstest |
| Cloudflare | WARP an/aus, Team-Name, Service-Token, Status, Neu verbinden |
| System | Passwort ändern, Neustart, Protokoll |

Kennt das kleine Sprachmodell dein Wort nicht, zeigt die Website das an. Dann hilft ein anderes Wort oder ein größeres Vosk-Modell, das du nach `/data/model` legst ([Liste der Modelle](https://alphacephei.com/vosk/models)).

## Selbst bauen

```sh
docker build -t ida-speaker .
```

Zum Entwickeln ohne Docker brauchst du Rust, `libasound2-dev`, `clang`, `meson`, `ninja` und [`libvosk.so`](https://github.com/alphacep/vosk-api/releases) in `/usr/local/lib`:

```sh
RUSTFLAGS="-L /usr/local/lib" cargo test
IDA_SPEAKER_DATA=./data VOSK_MODEL=./model cargo run
```

Mehr zum Aufbau steht in [docs/ARCHITEKTUR.md](docs/ARCHITEKTUR.md).

## Datenschutz

- Das Wake-Word wird nur auf dem Pi erkannt. Erst nach dem Wake-Word geht Audio zu IDA.
- Dieses Repo enthält keine Adressen, Schlüssel oder persönlichen Daten. Alles kommt von der Website und bleibt auf dem Gerät. In CI prüft ein Secret-Scan (gitleaks) jeden Push.

## Lizenz

MIT
