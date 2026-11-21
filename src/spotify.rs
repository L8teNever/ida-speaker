//! Spotify Connect über Spotify Soloist (offizieller Spotify-Client für Linux): der Lautsprecher erscheint in der
//! Spotify-App als eigenes Gerät.
//!
//! Soloist läuft im zweiten Container `ida-speaker-spotify` (Netzwerk des Pi, damit die Spotify-App ihn im WLAN findet;
//! Start und Download: docker/soloist.sh). Beide Container teilen sich das Volume `/data`:
//! - `/data/soloist/run.env`: Name und API-Schlüssel, schreibt dieses Modul; ändert er sich, startet Soloist neu.
//! - `/data/pulse/native`: PulseAudio aus diesem Container mit einer virtuellen Soundkarte `ida_music`. Soloist spielt
//!   dorthin, wir lesen sie mit `parec` mit und mischen die Musik selbst dazu – so wird sie beim Wake-Word leiser und der
//!   Echo-Filter kennt sie.
//! - `/data/soloist/ws.sock`: Soloists WebSocket-API (Pause, Weiter, Lied laden, Zustand), per socat durchgereicht.
//! - `/data/soloist/status`: letzte Meldung des Start-Skripts (Download, abgelaufen, Fehler).

use crate::dsp::Resampler;
use crate::state::App;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{Value, json};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// PulseAudio-Socket im geteilten Volume (startet entrypoint.sh).
pub const PULSE: &str = "unix:/data/pulse/native";
/// Virtuelle Soundkarte, in die Soloist spielt.
const SINK: &str = "ida_music";
/// So liest parec die Musik.
const RATE: u32 = 48_000;
/// So viel Musik darf höchstens vorrätig sein, bevor alte verworfen wird (Sekunden).
const MAX_AHEAD_S: f32 = 0.4;

#[derive(Debug, Clone, Serialize, Default)]
pub struct Status {
    pub enabled: bool,
    /// Name in der Spotify-App.
    pub name: String,
    /// Soloist läuft und ist bei Spotify angemeldet.
    pub connected: bool,
    /// Soloist läuft (WebSocket erreichbar), auch wenn noch niemand angemeldet ist.
    pub running: bool,
    /// Gerade das aktive Spotify-Gerät.
    pub active: bool,
    pub playing: bool,
    pub track: String,
    pub artist: String,
    /// Spotify-Lautstärke in Prozent.
    pub volume: u32,
    pub error: String,
}

/// Befehle an Spotify (Website, Sprachbefehle vom Server).
#[derive(Debug, Clone, PartialEq)]
pub enum Ctrl {
    Play,
    Pause,
    Toggle,
    Next,
    Prev,
    /// Lautstärke in Prozent.
    Volume(u32),
    /// Lauter (+) oder leiser (−) in Prozentpunkten.
    VolumeBy(i32),
    /// Spotify-URI abspielen (Lied, Album, Playlist, Künstler).
    Load(String),
}

impl Ctrl {
    pub fn parse(action: &str, value: Option<i64>, uri: &str) -> Option<Self> {
        Some(match action {
            "play" | "resume" => Ctrl::Play,
            "pause" | "stop" => Ctrl::Pause,
            "toggle" => Ctrl::Toggle,
            "next" => Ctrl::Next,
            "prev" | "previous" => Ctrl::Prev,
            "volume" => Ctrl::Volume(value?.clamp(0, 100) as u32),
            "louder" => Ctrl::VolumeBy(value.unwrap_or(10).clamp(1, 100) as i32),
            "quieter" => Ctrl::VolumeBy(-(value.unwrap_or(10).clamp(1, 100) as i32)),
            "load" if uri.starts_with("spotify:") => Ctrl::Load(uri.to_string()),
            _ => return None,
        })
    }

    /// Soloist-Befehle (WebSocket-API) für diesen Befehl, beim aktuellen Zustand.
    fn commands(&self, st: &Status) -> Vec<Value> {
        let cmd = |c: &str| json!({ "type": "command", "command": c });
        let activate = (!st.active).then(|| cmd("activate"));
        match self {
            Ctrl::Play => activate.into_iter().chain([cmd("play")]).collect(),
            Ctrl::Pause => vec![cmd("pause")],
            Ctrl::Toggle if st.playing => vec![cmd("pause")],
            Ctrl::Toggle => activate.into_iter().chain([cmd("play")]).collect(),
            Ctrl::Next => vec![cmd("skip_next")],
            Ctrl::Prev => vec![cmd("skip_prev")],
            Ctrl::Volume(p) => {
                vec![json!({ "type": "command", "command": "set_volume", "volume": (*p).min(100) })]
            }
            Ctrl::VolumeBy(d) => {
                let v = (st.volume as i32 + d).clamp(0, 100);
                vec![json!({ "type": "command", "command": "set_volume", "volume": v })]
            }
            Ctrl::Load(uri) => activate
                .into_iter()
                .chain([json!({ "type": "command", "command": "play", "uri": uri })])
                .collect(),
        }
    }
}

/// Name in der Spotify-App: eingestellt, sonst der Name des Lautsprechers, sonst der Hostname.
pub fn device_name(cfg: &crate::config::Config) -> String {
    let pick = [
        cfg.spotify.name.trim().to_string(),
        cfg.ida.device_name.trim().to_string(),
        std::fs::read_to_string("/etc/host-hostname")
            .unwrap_or_default()
            .trim()
            .to_string(),
    ];
    let name = pick
        .into_iter()
        .find(|s| !s.is_empty())
        .unwrap_or_else(|| "IDA Lautsprecher".into());
    let mut c = name.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => name,
    }
}

/// Inhalt von run.env für das Start-Skript (Shell, einfache Anführungszeichen). Leerer Schlüssel = Soloist aus.
pub fn run_env(name: &str, key: &str) -> String {
    let q = |s: &str| {
        s.chars()
            .filter(|c| !c.is_control() && *c != '\'')
            .collect::<String>()
    };
    format!("SOLOIST_NAME='{}'\nSOLOIST_KEY='{}'\n", q(name), q(key))
}

fn dir(app: &App) -> PathBuf {
    app.data.join("soloist")
}

/// Schreibt run.env nur bei Änderung (sonst startet Soloist unnötig neu).
fn write_run_env(app: &App, content: &str) {
    let d = dir(app);
    let path = d.join("run.env");
    if std::fs::read_to_string(&path).ok().as_deref() == Some(content) {
        return;
    }
    let tmp = d.join("run.env.tmp");
    let res = std::fs::create_dir_all(&d)
        .and_then(|_| std::fs::write(&tmp, content))
        .and_then(|_| std::fs::rename(&tmp, &path));
    if let Err(e) = res {
        app.log(format!(
            "Spotify: {} nicht schreibbar ({e})",
            path.display()
        ));
    }
}

fn set(app: &App, f: impl FnOnce(&mut Status)) {
    f(&mut app.status.lock().unwrap().spotify);
}

/// Startet Spotify: Einstellungen an das Start-Skript, Musik mitlesen, Steuerung über die WebSocket-API.
pub fn spawn(app: Arc<App>) {
    let reader = app.clone();
    std::thread::Builder::new()
        .name("spotify-audio".into())
        .spawn(move || music_reader(reader))
        .expect("spotify thread");
    tokio::spawn(run(app));
}

async fn run(app: Arc<App>) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    *app.music.lock().unwrap() = Some(tx);
    let mut cfg_rx = app.config.subscribe();
    let mut since = Instant::now(); // seit wann Soloist laufen sollte (für die Fehlermeldung)
    loop {
        let cfg = cfg_rx.borrow_and_update().clone();
        app.duck_pct.store(cfg.spotify.duck, Ordering::Relaxed);
        let name = device_name(&cfg);
        let on = cfg.spotify.enabled && !cfg.spotify.api_key.is_empty();
        write_run_env(
            &app,
            &run_env(&name, if on { &cfg.spotify.api_key } else { "" }),
        );
        set(&app, |s| {
            s.enabled = cfg.spotify.enabled;
            s.name = name.clone();
            if !on {
                *s = Status {
                    enabled: cfg.spotify.enabled,
                    name: name.clone(),
                    error: if cfg.spotify.enabled {
                        "Kein Soloist-API-Schlüssel eingetragen.".into()
                    } else {
                        String::new()
                    },
                    ..Default::default()
                };
            }
        });
        if !on {
            // Befehle verwerfen, bis Spotify an ist
            tokio::select! {
                r = cfg_rx.changed() => if r.is_err() { return },
                Some(_) = rx.recv() => app.log("Spotify ist aus (Audio → Spotify)"),
            }
            since = Instant::now();
            continue;
        }
        tokio::select! {
            r = cfg_rx.changed() => if r.is_err() { return },
            r = session(&app, &mut rx) => {
                set(&app, |s| { s.running = false; s.connected = false; s.playing = false; });
                let script = std::fs::read_to_string(dir(&app).join("status")).unwrap_or_default();
                let err = match r {
                    Err(_) if since.elapsed() > Duration::from_secs(90) && script.trim().is_empty() =>
                        "Soloist läuft nicht – fehlt der Container ida-speaker-spotify? Einmal ida-speaker-update ausführen.".into(),
                    Err(e) if !script.trim().is_empty() => format!("{} ({e:#})", script.trim()),
                    Err(e) => format!("Soloist startet … ({e:#})"),
                    Ok(()) => "Verbindung zu Soloist beendet".into(),
                };
                set(&app, |s| s.error = err);
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
}

/// Eine Verbindung zur WebSocket-API von Soloist. Endet, wenn sie abbricht.
async fn session(app: &Arc<App>, rx: &mut mpsc::UnboundedReceiver<Ctrl>) -> anyhow::Result<()> {
    let sock = dir(app).join("ws.sock");
    let stream = tokio::net::UnixStream::connect(&sock).await?;
    let (mut ws, _) = tokio_tungstenite::client_async("ws://localhost/", stream).await?;
    ws.send(Message::text(
        json!({ "type": "command", "command": "get_state" }).to_string(),
    ))
    .await?;
    set(app, |s| {
        s.running = true;
        s.error.clear();
    });
    app.log("Spotify: Soloist verbunden");
    loop {
        tokio::select! {
            msg = ws.next() => {
                let Some(msg) = msg else { return Ok(()) };
                if let Message::Text(t) = msg?
                    && let Ok(v) = serde_json::from_str::<Value>(&t)
                {
                    on_event(app, &v);
                }
            }
            Some(c) = rx.recv() => {
                let st = app.status.lock().unwrap().spotify.clone();
                if !st.connected {
                    app.log(format!("Spotify: noch nicht angemeldet – in der Spotify-App einmal „{}“ auswählen", st.name));
                }
                for cmd in c.commands(&st) {
                    ws.send(Message::text(cmd.to_string())).await?;
                }
            }
        }
    }
}

/// Titel und Künstler aus einem Soloist-`item` (Felder je nach Art unterschiedlich, daher tolerant).
pub fn item_info(item: &Value) -> (String, String) {
    let d = item.get("decorations").unwrap_or(item);
    let text = |v: &Value, keys: &[&str]| {
        keys.iter()
            .find_map(|k| v.get(*k).and_then(Value::as_str))
            .unwrap_or_default()
            .to_string()
    };
    let title = [d, item]
        .iter()
        .map(|v| text(v, &["name", "title", "track_name"]))
        .find(|s| !s.is_empty())
        .unwrap_or_default();
    let names = |v: &Value| -> String {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().or_else(|| x.get("name").and_then(Value::as_str)))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    };
    let artist = [d, item]
        .iter()
        .find_map(|v| {
            let s = v
                .get("artists")
                .map(names)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| text(v, &["artist", "artist_name", "show_name", "subtitle"]));
            (!s.is_empty()).then_some(s)
        })
        .unwrap_or_default();
    (title, artist)
}

fn on_event(app: &App, v: &Value) {
    let kind = v.get("type").and_then(Value::as_str).unwrap_or_default();
    let status = |v: &Value| {
        v.get("status")
            .and_then(Value::as_str)
            .map(|s| s == "playing")
    };
    let volume = |v: &Value| {
        v.get("volume")
            .and_then(Value::as_u64)
            .map(|n| n.min(100) as u32)
    };
    match kind {
        "auth_state" => {
            let logged = v.get("logged_in").and_then(Value::as_bool).unwrap_or(false);
            let active = v.get("is_active").and_then(Value::as_bool).unwrap_or(false);
            if logged && !app.status.lock().unwrap().spotify.connected {
                app.log("Spotify: angemeldet");
            }
            set(app, |s| {
                s.connected = logged;
                s.active = active;
                if !logged {
                    s.playing = false;
                }
            });
        }
        "playback_state" => set(app, |s| {
            s.connected = true;
            if let Some(p) = status(v) {
                s.playing = p;
            }
            if let Some(n) = volume(v) {
                s.volume = n;
            }
            if let Some(a) = v.get("is_active").and_then(Value::as_bool) {
                s.active = a;
            }
            if let Some(item) = v.get("item").filter(|i| !i.is_null()) {
                (s.track, s.artist) = item_info(item);
            }
        }),
        "playback_changed" => set(app, |s| {
            if let Some(p) = status(v) {
                s.playing = p;
            }
        }),
        "volume_changed" => set(app, |s| {
            if let Some(n) = volume(v) {
                s.volume = n;
            }
        }),
        "track_changed" => {
            if let Some(item) = v.get("item") {
                let (t, a) = item_info(item);
                set(app, |s| (s.track, s.artist) = (t, a));
            }
        }
        "device_changed" => {
            if let Some(a) = v.get("is_active").and_then(Value::as_bool) {
                set(app, |s| s.active = a);
            }
        }
        "error" => {
            let m = v.get("message").and_then(Value::as_str).unwrap_or("Fehler");
            app.log(format!("Spotify: {m}"));
        }
        _ => {}
    }
}

/// Liest die Musik von der virtuellen Soundkarte (parec) und legt sie als Mono in Geräterate in `Playback::music`.
fn music_reader(app: Arc<App>) {
    let mut warned = false;
    loop {
        let cfg = app.cfg();
        if !cfg.spotify.enabled || cfg.spotify.api_key.is_empty() || app.output_rate() == 0 {
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }
        match parec() {
            Ok(mut child) => {
                warned = false;
                let r = pump(&app, &mut child);
                let _ = child.kill();
                let _ = child.wait();
                if let Err(e) = r {
                    tracing::debug!("parec: {e}");
                }
            }
            Err(e) if !warned => {
                warned = true;
                app.log(format!("Spotify: Musik lässt sich nicht mitlesen ({e})"));
            }
            Err(_) => {}
        }
        app.playback.lock().unwrap().music.clear();
        std::thread::sleep(Duration::from_secs(3));
    }
}

fn parec() -> std::io::Result<Child> {
    Command::new("parec")
        .args([
            &format!("--server={}", PULSE.trim_start_matches("unix:")),
            &format!("--device={SINK}.monitor"),
            "--raw",
            "--format=float32le",
            &format!("--rate={RATE}"),
            "--channels=2",
            "--latency-msec=40",
            "--client-name=ida-speaker",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
}

/// Läuft, bis parec endet, Spotify ausgeschaltet wird oder sich die Ausgaberate ändert.
fn pump(app: &App, child: &mut Child) -> std::io::Result<()> {
    let mut out = child.stdout.take().ok_or(std::io::ErrorKind::BrokenPipe)?;
    let rate = app.output_rate();
    let mut rs = Resampler::new(RATE, rate);
    let mut buf = vec![0u8; 8 * 960]; // 20 ms Stereo-f32
    let mut fill = 0;
    let (mut mono, mut res) = (Vec::new(), Vec::new());
    let max = (rate as f32 * MAX_AHEAD_S) as usize;
    let mut checked = Instant::now();
    loop {
        let n = out.read(&mut buf[fill..])?;
        if n == 0 {
            return Ok(());
        }
        fill += n;
        let whole = fill / 8 * 8;
        mono.clear();
        mono.extend(buf[..whole].as_chunks::<8>().0.iter().map(|f| {
            let l = f32::from_le_bytes([f[0], f[1], f[2], f[3]]);
            let r = f32::from_le_bytes([f[4], f[5], f[6], f[7]]);
            (l + r) * 0.5
        }));
        buf.copy_within(whole..fill, 0);
        fill -= whole;
        res.clear();
        rs.process(&mono, &mut res);
        {
            let mut p = app.playback.lock().unwrap();
            p.music.extend(res.iter().copied());
            // Uhr der Soundkarte und der virtuellen Karte laufen minimal anders: zu viel Vorrat verwerfen
            if p.music.len() > max {
                let drop = p.music.len() - max / 2;
                p.music.drain(..drop);
            }
        }
        if checked.elapsed() > Duration::from_secs(1) {
            checked = Instant::now();
            let cfg = app.cfg();
            if !cfg.spotify.enabled || cfg.spotify.api_key.is_empty() || app.output_rate() != rate {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn befehle_vom_server() {
        assert_eq!(Ctrl::parse("pause", None, ""), Some(Ctrl::Pause));
        assert_eq!(
            Ctrl::parse("volume", Some(140), ""),
            Some(Ctrl::Volume(100))
        );
        assert_eq!(Ctrl::parse("volume", None, ""), None);
        assert_eq!(Ctrl::parse("quieter", None, ""), Some(Ctrl::VolumeBy(-10)));
        assert_eq!(
            Ctrl::parse("load", None, "spotify:track:abc"),
            Some(Ctrl::Load("spotify:track:abc".into()))
        );
        assert_eq!(Ctrl::parse("load", None, "https://evil"), None);
        assert_eq!(Ctrl::parse("tanzen", None, ""), None);
    }

    #[test]
    fn soloist_befehle() {
        let idle = Status::default();
        let names = |c: Ctrl, st: &Status| {
            c.commands(st)
                .iter()
                .map(|v| v["command"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(Ctrl::Play, &idle), ["activate", "play"]);
        let active = Status {
            active: true,
            playing: true,
            volume: 95,
            ..Default::default()
        };
        assert_eq!(names(Ctrl::Toggle, &active), ["pause"]);
        assert_eq!(names(Ctrl::Next, &active), ["skip_next"]);
        assert_eq!(Ctrl::VolumeBy(10).commands(&active)[0]["volume"], 100);
        let load = Ctrl::Load("spotify:track:x".into()).commands(&active);
        assert_eq!(load.len(), 1);
        assert_eq!(load[0]["uri"], "spotify:track:x");
    }

    #[test]
    fn titel_und_kuenstler() {
        let v: Value = serde_json::from_str(
            r#"{"uri":"spotify:track:1","decorations":{"name":"Flash Mich","artists":[{"name":"Mark Forster"}]}}"#,
        )
        .unwrap();
        assert_eq!(item_info(&v), ("Flash Mich".into(), "Mark Forster".into()));
        let v: Value =
            serde_json::from_str(r#"{"decorations":{"title":"Folge 1","artists":["A","B"]}}"#)
                .unwrap();
        assert_eq!(item_info(&v), ("Folge 1".into(), "A, B".into()));
        assert_eq!(item_info(&json!({})), (String::new(), String::new()));
    }

    #[test]
    fn einstellungen_fuer_das_skript() {
        assert_eq!(
            run_env("Nina's Küche", "ab\ncd"),
            "SOLOIST_NAME='Ninas Küche'\nSOLOIST_KEY='abcd'\n"
        );
    }

    #[test]
    fn name_fuer_die_app() {
        let mut c = crate::config::Config::default();
        c.ida.device_name = "küche".into();
        assert_eq!(device_name(&c), "Küche");
        c.spotify.name = "Nina Küche".into();
        assert_eq!(device_name(&c), "Nina Küche");
    }
}
