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

- **Adresse:** die Adresse des `voice`-Containers. Am besten ist die interne Adresse im Cloudflare-Netz, z. B. `http://10.x.x.x:8086` (siehe [CLOUDFLARE.md](CLOUDFLARE.md)). Die öffentliche HTTPS-Adresse des Voice-Dienstes geht auch.
- **Gerätename:** wie in `VOICE_DEVICES`
- **Schlüssel:** aus Schritt 2

„Verbindung testen“ holt ein Ticket. Klappt das, ist alles bereit.

## Was dabei passiert

1. `POST <adresse>/device/ticket` mit den Headern `x-device-name` und `x-device-key` liefert ein Ticket, das eine Minute gilt.
2. `WebSocket <adresse>/live?ticket=…&mode=speaker` öffnet das Gespräch:
   - Pi → IDA: binär PCM 16 bit LE, 16 kHz mono; Text `{"type":"end"}` zum Auflegen
   - IDA → Pi: binär PCM 16 bit LE, 24 kHz mono; Text `ready`, `text`, `tool`, `interrupted`, `end`, `error`
3. Mit `mode=speaker` antwortet Stella kurz und gesprochen und verweist nie auf einen Bildschirm. Statt einer langen Begrüßung meldet sie sich nur mit „Ja?“.
