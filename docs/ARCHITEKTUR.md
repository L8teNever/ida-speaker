# Aufbau

Ein Rust-Programm (`src/`), ein Prozess, drei Teile, die nebeneinander laufen:

| Datei | Aufgabe |
|---|---|
| `main.rs` | startet alles: Audio-Thread, Gespräch, WARP, Website |
| `config.rs` | Einstellungen in `/data/config.toml`; Geheimnisse werden auf der Website nie angezeigt |
| `audio.rs` | Mikrofon und Lautsprecher (cpal/ALSA); eigener Thread, der bei Änderungen alles neu aufbaut |
| `dsp.rs` | Abtastraten umrechnen, Pegel, Signaltöne, WebRTC-Echo-Filter (AEC3, Rauschunterdrückung, AGC) |
| `wake.rs` | Wake-Word mit Vosk: Grammatik aus dem Wort und `[unk]`, Sicherheit pro Wort gegen Fehlalarme |
| `session.rs` | ein Gespräch mit IDA: Ticket, WebSocket `/live?mode=speaker`, Stille-Erkennung, Auflegen |
| `warp.rs` | Cloudflare WARP: schreibt `mdm.xml`, startet `warp-svc`, liest den Status |
| `web.rs` + `web/index.html` | Einstellungs-Website mit Passwort (Argon2), Sitzungs-Cookie |

## Audio-Weg

```
Mikrofon (Geräterate) ─► mono ─► 16 kHz ─► 10-ms-Blöcke ─► AEC3.capture ─┬─► Wake-Word (wenn kein Gespräch)
                                                                        └─► IDA (im Gespräch)
IDA 24 kHz ─► Geräterate ─► Warteschlange ─► Lautsprecher
                                              └─► dasselbe Signal auf 16 kHz ─► AEC3.render (Referenz)
```

Das Referenzsignal für den Echo-Filter wird erst im Lautsprecher-Callback abgegriffen, also genau dann, wenn es wirklich rausgeht. So stimmt die Verzögerung, die AEC3 schätzt.

## Zustände

`starting` → `listening` ⇄ `connecting` → `talking` → `listening`, dazu `muted` (Wake-Word aus) und `error` (Audio-Gerät fehlt, neuer Versuch alle 3 s).

Während Signaltöne oder Stellas Stimme laufen, hört das Wake-Word nicht zu. So löst sie sich nicht selbst aus.
