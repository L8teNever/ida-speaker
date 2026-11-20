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
- USB-Mikrofon und Lautsprecher, oder eine USB-Freisprecheinrichtung. Als Lautsprecher geht auch eine **Bluetooth-Box**, die du auf der Website koppelst. Der Echo-Filter läuft in Software, deshalb reicht einfache Hardware. Stell Mikrofon und Lautsprecher am besten nicht direkt nebeneinander.
- Docker
- IDA mit eingerichtetem Voice-Dienst (`VOICE_URL`, `VOICE_SECRET`)

## Installieren

Auf dem Pi ein einziger Befehl:

```sh
curl -fsSL https://raw.githubusercontent.com/L8teNever/ida-speaker/main/install.sh | sh
```

Er installiert Docker, falls es fehlt, und richtet den Autostart ein: Nach jedem Neustart des Pi läuft ida-speaker von selbst. Außerdem schaltet er das WLAN frei und startet den Container. Alles Weitere geht über die Website.

Statt des Skripts geht auch die [`docker-compose.yml`](docker-compose.yml) aus diesem Repo (`docker compose up -d`).

Dann `http://<pi>:8080` oder `http://<hostname>.local:8080` öffnen. Beim ersten Mal führt ein **Einrichtungs-Assistent** Schritt für Schritt durch alles, mit Anleitung für IDA und Cloudflare:

1. **Passwort** für die Seite festlegen.
2. **WLAN** (falls der Pi noch am Kabel hängt oder im Einrichtungs-WLAN ist): Netz wählen, Passwort eingeben.
3. **IDA:** Adresse des Voice-Dienstes, Gerätename und Schlüssel eintragen, dann „Verbindung testen“ ([so geht's](docs/IDA.md)).
4. **Verbindung:** über die Domain (auch hinter Zero Trust, per Service-Token), über WARP ins private Netz oder im selben Netz. Cloudflare richtet „Automatisch einrichten“ mit einem API-Token ein ([was dabei passiert](docs/CLOUDFLARE.md)).
5. **Audio:** Mikrofon und Lautsprecher wählen, Testton abspielen, Pegel prüfen.
6. **Wake-Word:** Wort eintragen und Empfindlichkeit einstellen.

**Kein Netz?** Findet der Pi weder Kabel noch ein bekanntes WLAN, öffnet er nach etwa 90 Sekunden das WLAN **„IDA-Speaker-Setup“** (Passwort `idaspeaker`, beides änderbar). Mit dem Handy verbinden, `http://10.42.0.1:8080` öffnen und dort das richtige WLAN wählen.

Alle Änderungen gelten sofort, ein Neustart ist nicht nötig. Die Einstellungen liegen nur auf dem Pi (`/data/config.toml` im Volume), nie im Repo.

**Update:** `docker pull ghcr.io/l8tenever/ida-speaker:latest`, danach den Container neu erstellen (mit Compose: `docker compose pull && docker compose up -d`). Die Einstellungen bleiben erhalten.

## Bluetooth-Box

Unter **Audio → Bluetooth-Lautsprecher** drückst du „Geräte suchen“ und dann bei deiner Box „Koppeln“. Danach spricht Stella über die Box.

- Nach einem Neustart oder wenn die Box wieder angeht, verbindet sie sich von selbst.
- Ist die Box aus, läuft die Ausgabe über das normale Gerät.
- Das Mikrofon bleibt das am Pi.

Technisch läuft das über BlueZ und bluez-alsa auf dem Pi, beides richtet `install.sh` ein. Am besten nimmst du Raspberry Pi OS **Lite**. Auf der Desktop-Version übernimmt PipeWire Bluetooth.

## Nur wenn mein Handy da ist

Unter **Wake-Word → Nur wenn mein Handy da ist** koppelst du einmal dein Handy. Danach reagiert der Lautsprecher nur auf das Wake-Word, solange das Handy in der Nähe ist. Ist es weg, zeigt die Website „Handy weg“, und er hört nicht zu.

- Der Pi klopft alle paar Sekunden per Bluetooth beim Handy an.
- Antwortet es eine einstellbare Zeit nicht (Standard 60 s), gilt es als weg.
- Genaue Meter misst Bluetooth nicht. „Da“ heißt in Reichweite, in der Wohnung meist 5 bis 10 m. Ist die Signalstärke bekannt, kannst du mit „Mindest-Signal“ enger einstellen.
- Bluetooth am Handy einfach anlassen. Ein Gespräch per Knopf auf der Website geht immer.

## Zwei Namen

Unter **Wake-Word → Zweiter Name für schnelle Befehle** trägst du ein zweites Wort ein, z. B. „Computer“. Dahinter steckt eine eigene Persönlichkeit:

- **„Stella“:** das normale Gespräch, mit allem, was sie über dich weiß.
- **„Computer, Licht an“:** in einem Atemzug gesagt. Sie begrüßt dich nicht, erledigt den Befehl, antwortet in einem Satz („Licht ist an.“) und legt sofort auf.
- Was du direkt nach dem Namen sagst, hebt der Pi auf und schickt es gleich mit. So geht beim Verbinden kein Wort verloren.
- Einstellbar sind der Name im Gespräch, die Stimme (z. B. Puck), eigene Anweisungen, Smart Home direkt und „Nur ein Befehl“. Schaltest du „Nur ein Befehl“ aus, ist es ein normales Gespräch mit anderem Namen und anderer Stimme.
- Sagst du nach dem Namen nichts, legt sie nach ein paar Sekunden auf.

## Benutzen

- „Stella“ sagen (oder dein Wort). Ein kurzer Ton zeigt, dass sie zuhört, und sie meldet sich.
- Einfach reden. Du kannst sie jederzeit unterbrechen.
- Das Gespräch endet, wenn du dich verabschiedest, nach einer einstellbaren Zeit Stille oder über „Auflegen“ auf der Website. Ein zweiter Ton zeigt das Ende.
- Auf der Website kannst du ein Gespräch auch per Knopf starten oder das Wake-Word stummschalten.

## Einstellungen

| Bereich | Was |
|---|---|
| Wake-Word | Wort oder Phrase, Empfindlichkeit, an/aus, „Zuletzt gehört“ zum Testen, **zweiter Name** für schnelle Befehle |
| Audio | Geräte, Lautstärke, Mikrofon-Verstärkung, Signaltöne, Testton, Pegelanzeige, **Bluetooth-Box** suchen, koppeln, verbinden |
| Echo-Filter | Echo-Unterdrückung, Rauschunterdrückung, automatische Lautstärke, Notfall-Modus (Mikrofon stumm, solange Stella spricht) |
| Gespräch | Auflegen nach Stille, Höchstdauer, Sprach-Schwelle |
| IDA | Adresse, Gerätename, Schlüssel, Verbindungstest |
| Cloudflare | Einrichtung mit einem Klick per API-Token, oder von Hand: WARP an/aus, Team-Name, Service-Token, Status, Neu verbinden |
| WLAN | Netze suchen, verbinden, vergessen; Einrichtungs-WLAN ohne Netz |
| System | Passwort ändern, Programm neu starten, Pi neu starten oder ausschalten, Protokoll |

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
