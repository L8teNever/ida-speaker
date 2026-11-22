//! Bluetooth-Lautsprecher: suchen, koppeln, verbinden – über BlueZ des Pi (bluetoothctl über den D-Bus des Hosts).
//! Abgespielt wird über bluez-alsa: der gewählte Lautsprecher wird als ALSA-Gerät `ida_bluetooth` eingetragen.

use crate::state::App;
use serde::Serialize;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// ALSA-Gerät, unter dem der gewählte Lautsprecher erscheint.
pub const OUTPUT_ID: &str = "alsa:ida_bluetooth";

fn host_bus() -> Option<String> {
    let path = std::env::var("HOST_DBUS_SOCKET")
        .unwrap_or_else(|_| "/run/host-dbus/system_bus_socket".into());
    std::path::Path::new(&path)
        .exists()
        .then(|| format!("unix:path={path}"))
}

fn which(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

pub fn valid_mac(mac: &str) -> bool {
    let parts: Vec<&str> = mac.split(':').collect();
    parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Entfernt Farbcodes und Prompt-Reste aus der Ausgabe von bluetoothctl.
pub fn clean(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // ESC [ … Buchstabe
            for d in chars.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
        } else if c != '\u{1}' && c != '\u{2}' && c != '\r' {
            out.push(c);
        }
    }
    out.trim().to_string()
}

/// Ein einzelner Aufruf (`bluetoothctl devices`, `info …`).
async fn ctl(args: &[&str], timeout_s: u64) -> anyhow::Result<String> {
    let bus = host_bus().ok_or_else(|| {
        anyhow::anyhow!("Kein Zugriff auf Bluetooth des Pi (D-Bus des Hosts nicht eingebunden)")
    })?;
    let out = tokio::time::timeout(
        Duration::from_secs(timeout_s),
        Command::new("bluetoothctl")
            .env("DBUS_SYSTEM_BUS_ADDRESS", bus)
            .args(args)
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("bluetoothctl antwortet nicht"))??;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(clean)
        .collect::<Vec<_>>()
        .join("\n"))
}

/// Mehrere Befehle in einer Sitzung (für Koppeln mit Agent). Pro Schritt: Befehl, Erfolgs- und Fehlertexte, Wartezeit.
async fn session(steps: &[(String, &[&str], &[&str], u64)]) -> anyhow::Result<Vec<String>> {
    let bus = host_bus().ok_or_else(|| anyhow::anyhow!("Kein Zugriff auf Bluetooth des Pi"))?;
    let mut child = Command::new("bluetoothctl")
        .env("DBUS_SYSTEM_BUS_ADDRESS", bus)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child.stdin.take().expect("stdin");
    let mut lines = BufReader::new(child.stdout.take().expect("stdout")).lines();
    let mut log = Vec::new();
    for (cmd, ok, fail, wait) in steps {
        stdin.write_all(format!("{cmd}\n").as_bytes()).await?;
        if ok.is_empty() && fail.is_empty() {
            tokio::time::sleep(Duration::from_millis(*wait)).await;
            continue;
        }
        let done = tokio::time::timeout(Duration::from_secs(*wait), async {
            while let Ok(Some(raw)) = lines.next_line().await {
                let l = clean(&raw);
                if l.is_empty() {
                    continue;
                }
                log.push(l.clone());
                if ok.iter().any(|m| l.contains(m)) {
                    return Ok(());
                }
                if let Some(m) = fail.iter().find(|m| l.contains(*m)) {
                    return Err(anyhow::anyhow!("{cmd}: {}", explain(m, &l)));
                }
            }
            Err(anyhow::anyhow!("{cmd}: bluetoothctl beendet"))
        })
        .await;
        match done {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = stdin.write_all(b"quit\n").await;
                return Err(e);
            }
            Err(_) => {
                let _ = stdin.write_all(b"quit\n").await;
                anyhow::bail!(
                    "{cmd}: keine Antwort in {wait} s – ist der Lautsprecher im Kopplungsmodus und in der Nähe?"
                );
            }
        }
    }
    let _ = stdin.write_all(b"quit\n").await;
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    Ok(log)
}

fn explain(marker: &str, line: &str) -> String {
    if marker.contains("AuthenticationFailed") || marker.contains("AuthenticationRejected") {
        "Kopplung abgelehnt – Lautsprecher in den Kopplungsmodus bringen und noch einmal".into()
    } else if marker.contains("not available") {
        "Gerät nicht gefunden – Lautsprecher einschalten, Kopplungsmodus, neu suchen".into()
    } else {
        line.to_string()
    }
}

#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub struct Device {
    pub mac: String,
    pub name: String,
    pub paired: bool,
    pub trusted: bool,
    pub connected: bool,
    /// Kann Audio abspielen (A2DP-Sink oder Icon audio-*).
    pub audio: bool,
    pub icon: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Status {
    pub available: bool,
    pub error: String,
    /// Gewählter Lautsprecher
    pub mac: String,
    pub name: String,
    pub connected: bool,
}

/// `bluetoothctl devices` → (MAC, Name)
pub fn parse_devices(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let rest = l.trim().strip_prefix("Device ")?;
            let (mac, name) = rest.split_once(' ').unwrap_or((rest, ""));
            valid_mac(mac).then(|| (mac.to_uppercase(), name.trim().to_string()))
        })
        .collect()
}

/// `bluetoothctl info MAC` → Gerät
pub fn parse_info(mac: &str, text: &str) -> Device {
    let mut d = Device {
        mac: mac.to_uppercase(),
        ..Default::default()
    };
    for l in text.lines().map(str::trim) {
        let yes = |k: &str| l.strip_prefix(k).map(|v| v.trim() == "yes");
        if let Some(v) = l.strip_prefix("Alias:") {
            d.name = v.trim().to_string();
        } else if let Some(v) = l.strip_prefix("Name:").filter(|_| d.name.is_empty()) {
            d.name = v.trim().to_string();
        } else if let Some(v) = yes("Paired:") {
            d.paired = v;
        } else if let Some(v) = yes("Trusted:") {
            d.trusted = v;
        } else if let Some(v) = yes("Connected:") {
            d.connected = v;
        } else if let Some(v) = l.strip_prefix("Icon:") {
            d.icon = v.trim().to_string();
        }
        if l.starts_with("UUID:") && (l.contains("Audio Sink") || l.contains("0000110b")) {
            d.audio = true;
        }
    }
    if d.icon.starts_with("audio") {
        d.audio = true;
    }
    d
}

pub async fn available() -> Result<(), String> {
    if host_bus().is_none() {
        return Err("Container ohne D-Bus des Pi gestartet (siehe install.sh)".into());
    }
    if !which("bluetoothctl") {
        return Err("bluetoothctl fehlt im Image".into());
    }
    match ctl(&["show"], 8).await {
        Ok(t) if t.contains("Controller") => Ok(()),
        Ok(_) => Err("Kein Bluetooth auf dem Pi gefunden – läuft der Dienst bluetooth? (install.sh richtet ihn ein)".into()),
        Err(e) => Err(e.to_string()),
    }
}

pub async fn info(mac: &str) -> Device {
    parse_info(mac, &ctl(&["info", mac], 8).await.unwrap_or_default())
}

/// Sucht einige Sekunden und gibt alle bekannten Geräte zurück, Audio-Geräte zuerst.
pub async fn scan(seconds: u64) -> anyhow::Result<Vec<Device>> {
    ensure_powered().await.map_err(|e| anyhow::anyhow!(e))?;
    let _ = ctl(
        &["--timeout", &seconds.to_string(), "scan", "on"],
        seconds + 5,
    )
    .await;
    let list = parse_devices(&ctl(&["devices"], 8).await?);
    let infos = futures_util::future::join_all(list.iter().map(|(mac, _)| info(mac))).await;
    let mut out: Vec<Device> = list
        .into_iter()
        .zip(infos)
        .map(|((mac, name), mut d)| {
            if d.mac.is_empty() {
                d.mac = mac;
            }
            if d.name.is_empty() {
                d.name = name;
            }
            d
        })
        .filter(|d| !d.name.is_empty() && d.name.replace('-', ":") != d.mac)
        .collect();
    out.sort_by(|a, b| {
        b.connected
            .cmp(&a.connected)
            .then(b.audio.cmp(&a.audio))
            .then(b.paired.cmp(&a.paired))
            .then(a.name.cmp(&b.name))
    });
    Ok(out)
}

/// Koppeln (falls nötig), vertrauen (damit er sich nach dem Neustart wieder verbindet) und verbinden.
pub async fn pair_connect(mac: &str) -> anyhow::Result<()> {
    pair(mac, true).await
}

/// Koppeln und vertrauen; `connect` = danach auch verbinden (Lautsprecher), sonst nur gekoppelt lassen (Handy).
pub async fn pair(mac: &str, connect: bool) -> anyhow::Result<()> {
    ensure_powered().await.map_err(|e| anyhow::anyhow!(e))?;
    let known = info(mac).await;
    let mut steps: Vec<(String, &[&str], &[&str], u64)> = vec![
        ("power on".into(), &[][..], &[][..], 300),
        (
            "agent NoInputNoOutput".into(),
            &["Agent registered", "already registered"][..],
            &["Failed"][..],
            5,
        ),
        (
            "default-agent".into(),
            &["Default agent request successful"][..],
            &["Failed", "No agent"][..],
            5,
        ),
    ];
    if !known.paired {
        steps.push(("scan on".into(), &[][..], &[][..], 6000));
        steps.push((
            format!("pair {mac}"),
            &["Pairing successful", "AlreadyExists"][..],
            &[
                "Failed to pair",
                "AuthenticationFailed",
                "AuthenticationRejected",
                "not available",
                "ConnectionAttemptFailed",
            ][..],
            40,
        ));
        steps.push(("scan off".into(), &[][..], &[][..], 300));
    }
    if !known.trusted {
        steps.push((
            format!("trust {mac}"),
            &["trust succeeded"][..],
            &["Failed", "not available"][..],
            10,
        ));
    }
    if connect {
        steps.push((
            format!("connect {mac}"),
            &["Connection successful", "AlreadyConnected"][..],
            &["Failed to connect", "not available"][..],
            30,
        ));
    }
    session(&steps).await?;
    Ok(())
}

/// Ergebnis von „ist das Gerät in Reichweite?“
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Reach {
    /// Gerät hat geantwortet (verbunden oder hat auf die Anfrage reagiert).
    Near,
    /// Keine Antwort (aus, zu weit weg, Bluetooth aus).
    Away,
    /// Konnte nicht prüfen (Bluetooth des Pi fehlt …).
    Unknown,
}

/// Deutet die Antwort von `bluetoothctl connect` auf ein gekoppeltes Handy: Auch „kein passendes Profil“ heißt,
/// es hat geantwortet. Nur Seitenaufruf-Timeout, „Host is down“ und Co. heißen weg.
pub fn classify_connect(out: &str) -> Reach {
    let o = out.to_lowercase();
    if o.contains("connection successful")
        || o.contains("alreadyconnected")
        || o.contains("already connected")
    {
        return Reach::Near;
    }
    let away = [
        "page-timeout",
        "host is down",
        "connection timed out",
        "not available",
        "le-connection-abort",
        "br-connection-create-socket",
        "no route",
    ];
    if away.iter().any(|a| o.contains(a)) {
        return Reach::Away;
    }
    if o.contains("profile-unavailable")
        || o.contains("br-connection-refused")
        || o.contains("in progress")
        || o.contains("canceled")
    {
        return Reach::Near;
    }
    if o.contains("failed to connect") {
        Reach::Away
    } else {
        Reach::Unknown
    }
}

/// Signalstärke aus `bluetoothctl info` (nur vorhanden, wenn das Gerät gerade gesehen wurde).
pub fn parse_rssi(text: &str) -> Option<i32> {
    text.lines().find_map(|l| {
        let v = l.trim().strip_prefix("RSSI:")?.trim();
        // „-60“ oder „0xffffffc4 (-60)“
        let v = v
            .rsplit_once('(')
            .map(|(_, r)| r.trim_end_matches(')'))
            .unwrap_or(v);
        v.trim().parse().ok()
    })
}

/// Klopft kurz beim gekoppelten Handy an. War es nicht ohnehin verbunden, wird die Verbindung gleich wieder getrennt,
/// damit es nicht etwa seine Musik über den Pi abspielt.
pub async fn probe(mac: &str) -> (Reach, Option<i32>) {
    let text = ctl(&["info", mac], 8).await.unwrap_or_default();
    let rssi = parse_rssi(&text);
    if parse_info(mac, &text).connected {
        return (Reach::Near, rssi);
    }
    let out = match ctl(&["connect", mac], 12).await {
        Ok(o) => o,
        Err(_) => return (Reach::Away, rssi),
    };
    let reach = classify_connect(&out);
    if reach == Reach::Near {
        let _ = ctl(&["disconnect", mac], 8).await;
    }
    (reach, rssi)
}

pub async fn disconnect(mac: &str) -> anyhow::Result<()> {
    ctl(&["disconnect", mac], 15).await?;
    Ok(())
}

pub async fn remove(mac: &str) -> anyhow::Result<()> {
    ctl(&["remove", mac], 15).await?;
    Ok(())
}

/// ALSA-Gerät `ida_bluetooth` für den gewählten Lautsprecher (bluez-alsa). Leer = Gerät entfernen.
/// Fest auf Stereo: sonst stellt bluez-alsa die Verbindung bei Mono-Ton auf 1 Kanal um, und viele
/// günstige Boxen spielen dann zu schnell, zu hoch oder abgehackt.
pub fn asoundrc(mac: &str, name: &str) -> String {
    if !valid_mac(mac) {
        return String::new();
    }
    let name = name.replace(['"', '\\', '\n'], "");
    format!(
        "# von ida-speaker geschrieben – Bluetooth-Lautsprecher über bluez-alsa\n\
         pcm.ida_bluetooth {{\n  type plug\n  slave {{\n    pcm {{\n      type bluealsa\n      device \"{mac}\"\n      profile \"a2dp\"\n    }}\n    channels 2\n  }}\n  \
         hint {{\n    show on\n    description \"Bluetooth: {name}\"\n  }}\n}}\n"
    )
}

pub fn write_asoundrc(mac: &str, name: &str) {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let path = std::path::Path::new(&home).join(".asoundrc");
    let text = asoundrc(mac, name);
    let res = if text.is_empty() {
        std::fs::remove_file(&path).or(Ok(()))
    } else {
        std::fs::write(&path, text)
    };
    if let Err(e) = res {
        tracing::warn!("~/.asoundrc: {e}");
    }
}

/// Stellt sicher, dass der Bluetooth-Adapter des Pi an ist (`Powered: yes`), und versucht es sonst bis zu 3-mal.
/// Gibt Fehlertext zurück, wenn er sich nicht einschalten lässt (z. B. per rfkill gesperrt).
pub async fn ensure_powered() -> Result<(), String> {
    let mut last = String::new();
    for _ in 0..3 {
        let show = ctl(&["show"], 8).await.map_err(|e| e.to_string())?;
        if !show.contains("Controller") {
            return Err("Kein Bluetooth-Adapter gefunden".into());
        }
        if show.lines().any(|l| l.trim() == "Powered: yes") {
            return Ok(());
        }
        last = ctl(&["power", "on"], 8).await.unwrap_or_default();
        tokio::time::sleep(Duration::from_millis(800)).await;
    }
    let l = last.to_lowercase();
    Err(if l.contains("blocked") {
        "Bluetooth ist per rfkill gesperrt – am Pi: sudo rfkill unblock bluetooth".into()
    } else {
        format!("Bluetooth-Adapter lässt sich nicht einschalten: {last}")
    })
}

/// Verbindet den gewählten Lautsprecher (ohne neu zu koppeln): Bluetooth einschalten, vertrauen, verbinden.
/// Gibt zurück, ob er danach verbunden ist.
pub async fn reconnect(mac: &str) -> bool {
    if let Err(e) = ensure_powered().await {
        tracing::warn!("Bluetooth: {e}");
        return false;
    }
    let known = info(mac).await;
    if known.connected {
        return true;
    }
    if known.mac.is_empty() || !known.paired {
        // BlueZ kennt die Box nicht (mehr) gekoppelt: kurz suchen, vielleicht ist sie im Kopplungsmodus
        let _ = ctl(&["--timeout", "5", "scan", "on"], 10).await;
    }
    if !known.trusted {
        let _ = ctl(&["trust", mac], 8).await;
    }
    let out = ctl(&["connect", mac], 20).await.unwrap_or_default();
    if out.contains("not-powered") || out.contains("NotReady") {
        // Adapter ging zwischendurch aus: einschalten und gleich noch einmal
        if ensure_powered().await.is_ok() {
            let _ = ctl(&["connect", mac], 20).await;
        }
    }
    info(mac).await.connected
}

/// Wartezeit zwischen zwei Verbindungsversuchen: erst alle 10 s, nach 3 Minuten alle 30 s – gibt nie auf.
fn retry_wait(tries: u32) -> Duration {
    Duration::from_secs(if tries <= 18 { 10 } else { 30 })
}

/// Wächter: hält den gewählten Lautsprecher verbunden (auch nach Strom weg, Reichweite, Neustart der Box)
/// und baut das Audio neu auf, sobald er (wieder) da ist oder weg ist.
pub async fn run(app: Arc<App>) {
    let cfg = app.cfg();
    write_asoundrc(&cfg.audio.bluetooth, &cfg.audio.bluetooth_name);
    let mut was_connected = false;
    let mut tries = 0u32;
    let mut next_try = tokio::time::Instant::now();
    loop {
        // Bluetooth des Pi kann später starten als der Container: bis es da ist, jedes Mal nachsehen
        if !app.status.lock().unwrap().bluetooth.available {
            let ok = available().await;
            let mut s = app.status.lock().unwrap();
            s.bluetooth.available = ok.is_ok();
            s.bluetooth.error = ok.err().unwrap_or_default();
        }
        let cfg = app.cfg();
        let mac = cfg.audio.bluetooth.clone();
        let mut connected = false;
        if !mac.is_empty() && app.status.lock().unwrap().bluetooth.available {
            connected = info(&mac).await.connected;
            if connected {
                tries = 0;
            } else if tokio::time::Instant::now() >= next_try {
                tries += 1;
                connected = reconnect(&mac).await;
                next_try = tokio::time::Instant::now() + retry_wait(tries);
                if connected {
                    tries = 0;
                }
            }
            if connected != was_connected {
                app.log(if connected {
                    format!("Bluetooth: „{}“ verbunden", cfg.audio.bluetooth_name)
                } else {
                    format!(
                        "Bluetooth: „{}“ getrennt – spiele über das Standardgerät, versuche es weiter",
                        cfg.audio.bluetooth_name
                    )
                });
                {
                    let mut s = app.status.lock().unwrap();
                    s.bluetooth.connected = connected;
                }
                if connected {
                    // bluez-alsa braucht einen Moment, bis die Box als Audio-Gerät da ist
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                app.rebuild_audio.store(true, Ordering::Relaxed);
            }
        } else {
            tries = 0;
            next_try = tokio::time::Instant::now();
        }
        was_connected = connected;
        {
            let mut s = app.status.lock().unwrap();
            s.bluetooth.mac = mac;
            s.bluetooth.name = cfg.audio.bluetooth_name.clone();
            s.bluetooth.connected = connected;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_devices_and_info() {
        let list = "Device 11:22:33:AA:BB:CC JBL Flip 6\nDevice 44:55:66:77:88:99 Handy\nGarbage\nDevice zz Bad";
        assert_eq!(
            parse_devices(list),
            vec![
                ("11:22:33:AA:BB:CC".into(), "JBL Flip 6".into()),
                ("44:55:66:77:88:99".into(), "Handy".into())
            ]
        );
        let info = "Device 11:22:33:AA:BB:CC (public)\n\tName: JBL Flip 6\n\tAlias: Küchenbox\n\tIcon: audio-card\n\tPaired: yes\n\tTrusted: no\n\tConnected: yes\n\tUUID: Audio Sink                (0000110b-0000-1000-8000-00805f9b34fb)";
        let d = parse_info("11:22:33:aa:bb:cc", info);
        assert_eq!(d.name, "Küchenbox");
        assert!(d.paired && d.connected && !d.trusted && d.audio);
        assert_eq!(d.mac, "11:22:33:AA:BB:CC");
    }

    #[test]
    fn presence_probe_parsing() {
        assert_eq!(
            classify_connect("Attempting to connect to X\nConnection successful"),
            Reach::Near
        );
        assert_eq!(
            classify_connect(
                "Failed to connect: org.bluez.Error.Failed br-connection-profile-unavailable"
            ),
            Reach::Near
        );
        assert_eq!(
            classify_connect(
                "Failed to connect: org.bluez.Error.Failed br-connection-page-timeout"
            ),
            Reach::Away
        );
        assert_eq!(
            classify_connect("Failed to connect: org.bluez.Error.Failed Host is down"),
            Reach::Away
        );
        assert_eq!(
            classify_connect("Device 11:22:33:44:55:66 not available"),
            Reach::Away
        );
        assert_eq!(classify_connect(""), Reach::Unknown);
        assert_eq!(parse_rssi("\tRSSI: -61"), Some(-61));
        assert_eq!(parse_rssi("\tRSSI: 0xffffffc4 (-60)"), Some(-60));
        assert_eq!(parse_rssi("\tConnected: no"), None);
    }

    #[test]
    fn cleans_ansi_and_prompt() {
        assert_eq!(
            clean("\u{1b}[0;94m[bluetooth]\u{1b}[0m# Pairing successful\r"),
            "[bluetooth]# Pairing successful"
        );
    }

    #[test]
    fn mac_and_asoundrc() {
        assert!(valid_mac("11:22:33:AA:bb:CC"));
        assert!(!valid_mac("11:22:33:AA:BB"));
        assert!(!valid_mac("11:22:33:AA:BB:\"x"));
        let rc = asoundrc("11:22:33:AA:BB:CC", "Box \"1\"");
        assert!(
            rc.contains("device \"11:22:33:AA:BB:CC\"")
                && rc.contains("Bluetooth: Box 1")
                && rc.contains("type bluealsa")
                && rc.contains("channels 2")
        );
        assert_eq!(asoundrc("kaputt", "x"), "");
    }
}
