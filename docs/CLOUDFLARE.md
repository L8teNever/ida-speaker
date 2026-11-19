# Cloudflare Zero Trust (WARP im Container)

Damit der Lautsprecher IDA über die **interne** Adresse erreicht, ohne öffentliche Domain und ohne offenen Port, läuft im Container der Cloudflare-WARP-Client. Er meldet sich per Service-Token an, ganz ohne Browser.

Auf dem Pi selbst richtest du nichts ein. WARP leitet nur den Verkehr des Containers um, nicht den des ganzen Pi.

## Voraussetzungen in Cloudflare

1. **Tunnel zum IDA-Server** mit einer privaten Netz-Route, z. B. `10.0.0.0/24` oder die Docker-Netze des Servers (*Networks → Tunnels → Private networks*).
2. **Service-Token** anlegen: *Access → Service auth → Service tokens → Create*. Client-ID und Client-Secret notieren, das Secret wird nur einmal angezeigt.
3. **Geräte-Anmeldung per Service-Token erlauben:** *Settings → WARP Client → Device enrollment permissions → Policy* mit der Aktion **Service Auth** und dem Token als Regel.
4. **Split Tunnels:** Standardmäßig *schließt* WARP private Bereiche (`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`) aus. Liegt IDA in so einem Bereich, nimm genau diesen Bereich aus der Ausschlussliste heraus, oder stell das Geräteprofil auf *Include* mit dem IDA-Netz um. Sonst geht der Verkehr nicht durch den Tunnel.
   Tipp: Lass das Heimnetz (z. B. `192.168.0.0/16`) ausgeschlossen, damit die Website des Pi im WLAN erreichbar bleibt.

## Auf der Lautsprecher-Website

Unter **Cloudflare**:

- „WARP verwenden“ einschalten
- **Team-Name:** der Teil vor `.cloudflareaccess.com`
- **Client-ID** und **Client-Secret** des Service-Tokens

Nach dem Speichern schreibt der Lautsprecher `/var/lib/cloudflare-warp/mdm.xml` und startet `warp-svc` neu. Nach ein paar Sekunden steht dort „Connected“. Danach unter **IDA** die interne Adresse eintragen und „Verbindung testen“ drücken.

## Wenn's hakt

- **„nicht verbunden“:** Client-ID und Secret prüfen, ebenso die Enrollment-Policy (Aktion *Service Auth*).
- **Verbunden, aber IDA nicht erreichbar:** Split Tunnels prüfen (Punkt 4) und ob die Tunnel-Route das Ziel abdeckt.
- **WARP startet gar nicht:** Der Container braucht `--cap-add NET_ADMIN`, `--device /dev/net/tun` und `--sysctl net.ipv4.conf.all.src_valid_mark=1`.
- Das Protokoll unter **System** zeigt die Ausgabe von `warp-cli status`.
