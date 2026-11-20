# Lautsprecher in IDA anmelden

Der Lautsprecher nutzt den Voice-Dienst von IDA, also denselben Dienst wie das Live-Gespräch in der App. Er meldet sich mit eigenem Namen und Schlüssel an. So lässt sich jeder Lautsprecher einzeln sperren.

## 1. Gerät in IDA eintragen

In der `.env` von IDA:

```env
VOICE_DEVICES=wohnzimmer            # mehrere mit Komma: wohnzimmer,kueche
```

Danach IDA und den `voice`-Container neu starten (`docker compose up -d`).

Erlaubt sind Kleinbuchstaben, Ziffern, `-` und `_`. Wer einen Namen wieder streicht, sperrt das Gerät sofort.

## 2. Schlüssel holen

Angemeldet in IDA:

```
GET /api/v1/voice/devices
```

Die Antwort sieht so aus: `{"devices": [{"name": "wohnzimmer", "key": "…"}]}`. Der Schlüssel wird aus `VOICE_SECRET` und dem Namen berechnet. Er bleibt also gleich, bis sich `VOICE_SECRET` ändert.

## 3. Auf der Lautsprecher-Website eintragen

Unter **IDA**:

- **Adresse:** die Adresse des `voice`-Containers im Cloudflare-Netz. Das ist entweder die Server-IP mit dem veröffentlichten Port (z. B. `http://10.x.x.x:8086`) oder die Docker-IP des Containers mit Port 8000 (z. B. `http://172.x.x.x:8000`, zu finden mit `docker inspect`). Siehe [CLOUDFLARE.md](CLOUDFLARE.md). Die öffentliche HTTPS-Adresse des Voice-Dienstes geht auch.
- **Gerätename:** wie in `VOICE_DEVICES`
- **Schlüssel:** aus Schritt 2

„Verbindung testen“ holt ein Ticket. Klappt das, ist alles bereit.

## Eigene Anweisungen und Smart Home

Unter **IDA → Anweisungen für diesen Lautsprecher**:

- **Eigener Prompt:** Er kommt nur in Gespräche über diesen Lautsprecher, zusätzlich zu Stellas normalen Regeln. App und Telefon bleiben unverändert. Gut geeignet für Raum, Gerätenamen aus Home Assistant und Wünsche zum Stil, z. B. „Ich stehe im Wohnzimmer. Licht heißt `light.wohnzimmer`.“
- **Smart Home direkt:** Stella hat die Home-Assistant-Tools direkt zur Hand und schaltet auf „Licht aus“ sofort, ohne nachzufragen (an, aus, umschalten). Alles andere, etwa Automationen ändern oder löschen, fragt weiter nach.

Beides schickt der Lautsprecher bei jedem Ticket mit, IDA speichert es nicht.

## Was dabei passiert

1. `POST <adresse>/device/ticket` mit den Headern `x-device-name` und `x-device-key` und optional dem Body `{"prompt": …, "smart_home": true}` liefert ein Ticket, das eine Minute gilt. Beim zweiten Namen kommen `"name"` (so heißt sie im Gespräch), `"voice"` (Gemini-Stimme) und `"oneshot": true` dazu: keine Begrüßung, ein Befehl, eine kurze Antwort, dann legt sie auf. Was nach dem Namen schon gesagt wurde, schickt der Pi direkt nach `ready` als erstes Audio.
2. `WebSocket <adresse>/live?ticket=…&mode=speaker` öffnet das Gespräch:
   - Pi → IDA: binär PCM 16 bit LE, 16 kHz mono; Text `{"type":"end"}` zum Auflegen
   - IDA → Pi: binär PCM 16 bit LE, 24 kHz mono; Text `ready`, `text`, `tool`, `interrupted`, `end`, `error`
3. Mit `mode=speaker` antwortet Stella kurz und gesprochen und verweist nie auf einen Bildschirm. Statt einer langen Begrüßung meldet sie sich nur mit „Ja?“.
