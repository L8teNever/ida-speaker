//! WLAN des Pi über NetworkManager (nmcli über den D-Bus des Hosts, siehe docker-compose.yml).
//! Ohne Netz öffnet der Lautsprecher ein Einrichtungs-WLAN; darüber erreicht man die Website und wählt das richtige WLAN.

use crate::state::App;
use serde::Serialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Command;

/// Name der Hotspot-Verbindung in NetworkManager.
pub const HOTSPOT: &str = "ida-speaker-setup";

fn host_bus() -> Option<String> {
    let path = std::env::var("HOST_DBUS_SOCKET")
        .unwrap_or_else(|_| "/run/host-dbus/system_bus_socket".into());
    std::path::Path::new(&path)
        .exists()
        .then(|| format!("unix:path={path}"))
}

pub fn available() -> bool {
    host_bus().is_some() && which("nmcli")
}

fn which(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

/// Führt nmcli gegen den NetworkManager des Hosts aus.
pub async fn nmcli(args: &[&str]) -> anyhow::Result<String> {
    let bus = host_bus().ok_or_else(|| {
        anyhow::anyhow!("Kein Zugriff auf das Netzwerk des Pi (D-Bus des Hosts nicht eingebunden)")
    })?;
    let out = tokio::time::timeout(
        Duration::from_secs(45),
        Command::new("nmcli")
            .env("DBUS_SYSTEM_BUS_ADDRESS", bus)
            .args(args)
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("nmcli antwortet nicht"))??;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() {
        Ok(text)
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        anyhow::bail!("{}", if err.is_empty() { text } else { err })
    }
}

/// Zerlegt eine Zeile von `nmcli -t` (Trenner `:`, maskiert als `\:`).
pub fn split_terse(line: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut esc = false;
    for c in line.chars() {
        match (esc, c) {
            (true, c) => {
                out.last_mut().unwrap().push(c);
                esc = false;
            }
            (false, '\\') => esc = true,
            (false, ':') => out.push(String::new()),
            (false, c) => out.last_mut().unwrap().push(c),
        }
    }
    out
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Network {
    pub ssid: String,
    pub signal: u32,
    pub security: String,
    pub in_use: bool,
    pub saved: bool,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Status {
    pub available: bool,
    pub ssid: String,
    pub hotspot: bool,
    pub ethernet: bool,
    pub ip: String,
    pub hostname: String,
    pub error: String,
}

/// Netze in Reichweite, stärkstes zuerst, jedes nur einmal.
pub fn parse_networks(list: &str, saved: &[String]) -> Vec<Network> {
    let mut nets: Vec<Network> = Vec::new();
    for line in list.lines() {
        let f = split_terse(line);
        if f.len() < 4 || f[1].is_empty() {
            continue;
        }
        let n = Network {
            in_use: f[0].trim() == "*",
            ssid: f[1].clone(),
            signal: f[2].trim().parse().unwrap_or(0),
            security: f[3].trim().to_string(),
            saved: saved.contains(&f[1]),
        };
        match nets.iter_mut().find(|x| x.ssid == n.ssid) {
            Some(x) => {
                x.in_use |= n.in_use;
                x.signal = x.signal.max(n.signal);
            }
            None => nets.push(n),
        }
    }
    nets.sort_by(|a, b| b.in_use.cmp(&a.in_use).then(b.signal.cmp(&a.signal)));
    nets
}

/// Gespeicherte WLAN-Verbindungen: (Name, SSID). Der Hotspot zählt nicht.
async fn saved_wifi() -> Vec<(String, String)> {
    let Ok(list) = nmcli(&["-t", "-f", "NAME,TYPE", "connection", "show"]).await else {
        return vec![];
    };
    let mut out = Vec::new();
    for line in list.lines() {
        let f = split_terse(line);
        if f.len() >= 2 && f[1].contains("wireless") && f[0] != HOTSPOT {
            let ssid = nmcli(&["-g", "802-11-wireless.ssid", "connection", "show", &f[0]])
                .await
                .unwrap_or_else(|_| f[0].clone());
            out.push((f[0].clone(), ssid));
        }
    }
    out
}

pub async fn status() -> Status {
    let mut st = Status {
        available: available(),
        hostname: hostname(),
        ..Default::default()
    };
    if !st.available {
        st.error = "WLAN-Steuerung nicht verfügbar – Container ohne D-Bus des Pi gestartet (siehe install.sh)".into();
        return st;
    }
    match nmcli(&[
        "-t",
        "-f",
        "NAME,TYPE,DEVICE",
        "connection",
        "show",
        "--active",
    ])
    .await
    {
        Ok(list) => {
            for line in list.lines() {
                let f = split_terse(line);
                if f.len() < 3 {
                    continue;
                }
                if f[0] == HOTSPOT {
                    st.hotspot = true;
                } else if f[1].contains("wireless") {
                    st.ssid = nmcli(&["-g", "802-11-wireless.ssid", "connection", "show", &f[0]])
                        .await
                        .unwrap_or_else(|_| f[0].clone());
                } else if f[1].contains("ethernet") {
                    st.ethernet = true;
                }
            }
        }
        Err(e) => st.error = e.to_string(),
    }
    if let Ok(ips) = nmcli(&["-g", "IP4.ADDRESS", "device", "show"]).await {
        st.ip = ips
            .split(['\n', '|'])
            .map(str::trim)
            .find(|a| !a.is_empty() && !a.starts_with("127."))
            .unwrap_or("")
            .split('/')
            .next()
            .unwrap_or("")
            .to_string();
    }
    st
}

pub async fn scan() -> anyhow::Result<Vec<Network>> {
    let saved: Vec<String> = saved_wifi().await.into_iter().map(|(_, s)| s).collect();
    let list = nmcli(&[
        "-t",
        "-f",
        "IN-USE,SSID,SIGNAL,SECURITY",
        "device",
        "wifi",
        "list",
        "--rescan",
        "auto",
    ])
    .await?;
    Ok(parse_networks(&list, &saved))
}

/// Verbindet mit einem WLAN. Läuft die Seite gerade über den Hotspot, bricht die Verbindung dabei ab (gleiches Funkmodul).
pub async fn connect(ssid: &str, password: &str) -> anyhow::Result<String> {
    if saved_wifi().await.iter().any(|(_, s)| s == ssid) && password.is_empty() {
        let name = saved_wifi()
            .await
            .into_iter()
            .find(|(_, s)| s == ssid)
            .map(|(n, _)| n)
            .unwrap_or_default();
        return nmcli(&["connection", "up", &name]).await;
    }
    let mut args = vec!["device", "wifi", "connect", ssid];
    if !password.is_empty() {
        args.extend(["password", password]);
    }
    nmcli(&args).await
}

pub async fn forget(ssid: &str) -> anyhow::Result<()> {
    for (name, s) in saved_wifi().await {
        if s == ssid {
            nmcli(&["connection", "delete", &name]).await?;
        }
    }
    Ok(())
}

async fn hotspot_up(ssid: &str, password: &str) -> anyhow::Result<()> {
    let _ = nmcli(&["connection", "delete", HOTSPOT]).await;
    nmcli(&[
        "device", "wifi", "hotspot", "con-name", HOTSPOT, "ssid", ssid, "password", password,
    ])
    .await?;
    Ok(())
}

async fn hotspot_down() {
    let _ = nmcli(&["connection", "down", HOTSPOT]).await;
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/host-hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Wächter: ohne LAN und WLAN öffnet er nach einer Weile den Hotspot. Gibt es gespeicherte WLANs, macht er ihn
/// alle 5 Minuten kurz zu, damit der Pi sich wieder mit ihnen verbinden kann.
pub async fn run(app: Arc<App>) {
    if !available() {
        app.log("WLAN-Steuerung nicht verfügbar (D-Bus des Pi nicht eingebunden)");
        return;
    }
    let mut offline_since: Option<Instant> = None;
    let mut hotspot_since: Option<Instant> = None;
    let grace = Duration::from_secs(
        std::env::var("HOTSPOT_AFTER_S")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(90),
    );
    loop {
        tokio::time::sleep(Duration::from_secs(15)).await;
        let cfg = app.cfg().wifi;
        let st = status().await;
        {
            let mut s = app.status.lock().unwrap();
            s.wifi = st.clone();
        }
        if !st.error.is_empty() {
            continue;
        }
        let online = st.ethernet || !st.ssid.is_empty();
        if st.hotspot {
            let since = *hotspot_since.get_or_insert_with(Instant::now);
            if !cfg.hotspot || online {
                hotspot_down().await;
                hotspot_since = None;
            } else if since.elapsed() > Duration::from_secs(300)
                && !saved_wifi().await.is_empty()
                && !app.web_active()
            {
                app.log("Einrichtungs-WLAN kurz aus – suche bekannte WLANs");
                hotspot_down().await;
                hotspot_since = None;
                offline_since = Some(Instant::now());
            }
            continue;
        }
        hotspot_since = None;
        if online {
            offline_since = None;
            continue;
        }
        let since = *offline_since.get_or_insert_with(Instant::now);
        if cfg.hotspot && since.elapsed() > grace {
            app.log(format!(
                "Kein Netz – öffne Einrichtungs-WLAN „{}“ (Website: http://10.42.0.1:8080)",
                cfg.hotspot_ssid
            ));
            if let Err(e) = hotspot_up(&cfg.hotspot_ssid, &cfg.hotspot_password).await {
                app.log(format!("Einrichtungs-WLAN geht nicht: {e}"));
            }
            offline_since = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terse_split_handles_escapes() {
        assert_eq!(
            split_terse(r"*:Mein\:WLAN:72:WPA2"),
            vec!["*", "Mein:WLAN", "72", "WPA2"]
        );
        assert_eq!(split_terse("a::b"), vec!["a", "", "b"]);
    }

    #[test]
    fn networks_dedup_and_sort() {
        let list =
            " :Nachbar:40:WPA2\n*:Zuhause:70:WPA2\n :Zuhause:80:WPA2\n ::30:WPA2\n :Offen:90:";
        let n = parse_networks(list, &["Nachbar".to_string()]);
        assert_eq!(
            n.iter().map(|x| x.ssid.as_str()).collect::<Vec<_>>(),
            vec!["Zuhause", "Offen", "Nachbar"]
        );
        assert!(n[0].in_use);
        assert_eq!(n[0].signal, 80);
        assert!(n[2].saved);
        assert_eq!(n[1].security, "");
    }
}
