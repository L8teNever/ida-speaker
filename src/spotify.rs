//! Spotify Connect über librespot: der Lautsprecher erscheint in der Spotify-App als eigenes Gerät.
//!
//! Die Musik läuft nicht über ein eigenes Audio-Gerät, sondern landet in `Playback::music` und wird im Lautsprecher
//! dazugemischt. So wird sie beim Wake-Word leiser, und der Echo-Filter kennt sie (sonst hört er die Musik als Sprache).
//!
//! Erste Anmeldung: in der Spotify-App das Gerät auswählen (Zeroconf über avahi des Pi). Danach liegen die Zugangsdaten
//! in `/data/spotify` und der Lautsprecher meldet sich nach jedem Neustart selbst wieder an.

use crate::dsp::Resampler;
use crate::state::App;
use futures_util::StreamExt;
use librespot::connect::{ConnectConfig, LoadRequest, LoadRequestOptions, Spirc};
use librespot::core::authentication::Credentials;
use librespot::core::cache::Cache;
use librespot::core::config::{DeviceType, SessionConfig};
use librespot::core::session::Session;
use librespot::playback::audio_backend::{Sink, SinkResult};
use librespot::playback::config::PlayerConfig;
use librespot::playback::convert::Converter;
use librespot::playback::decoder::AudioPacket;
use librespot::playback::mixer::{self, MixerConfig};
use librespot::playback::player::{Player, PlayerEvent};
use serde::Serialize;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Port des Anmelde-Servers für die Spotify-App (in install.sh freigegeben).
pub const PORT: u16 = 8082;
/// Spotify liefert immer 44,1 kHz Stereo.
const SPOTIFY_RATE: u32 = 44_100;
/// So viel Musik liegt höchstens im Voraus bereit (Sekunden). Mehr = Pause/Weiter reagieren später.
const AHEAD_S: f32 = 0.3;

#[derive(Debug, Clone, Serialize, Default)]
pub struct Status {
    pub enabled: bool,
    /// Name in der Spotify-App.
    pub name: String,
    /// Angemeldet und als Gerät bei Spotify sichtbar.
    pub connected: bool,
    pub user: String,
    /// Über die Spotify-App findbar (Zeroconf).
    pub discovery: bool,
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

/// Feste Geräte-ID aus dem Namen (40 Hex-Zeichen), damit die App nach einem Neustart nicht ein zweites Gerät zeigt.
pub fn device_id(name: &str) -> String {
    let mut out = String::with_capacity(40);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for round in 0u64..3 {
        for b in name.to_lowercase().bytes().chain(round.to_le_bytes()) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        out.push_str(&format!("{h:016x}"));
    }
    out.truncate(40);
    out
}

/// Startet Spotify in einem eigenen Thread (librespot will eine eigene Laufzeit) und baut es bei Änderungen neu auf.
pub fn spawn(app: Arc<App>) {
    std::thread::Builder::new()
        .name("spotify".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("spotify runtime");
            // LocalSet: alte Spotify-Verbindungen laufen nach dem Abmelden noch mit spawn_local aus
            tokio::task::LocalSet::new().block_on(&rt, run(app));
        })
        .expect("spotify thread");
}

async fn run(app: Arc<App>) {
    let mut cfg_rx = app.config.subscribe();
    loop {
        let cfg = cfg_rx.borrow_and_update().clone();
        app.duck_pct.store(cfg.spotify.duck, Ordering::Relaxed);
        let name = device_name(&cfg);
        let key = (cfg.spotify.enabled, name.clone());
        {
            let mut s = app.status.lock().unwrap();
            s.spotify = Status {
                enabled: cfg.spotify.enabled,
                name: name.clone(),
                ..Default::default()
            };
        }
        if !cfg.spotify.enabled {
            if cfg_rx.changed().await.is_err() {
                return;
            }
            continue;
        }
        let (tx, rx) = mpsc::unbounded_channel();
        *app.music.lock().unwrap() = Some(tx);
        let restart = async {
            loop {
                if cfg_rx.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
                let next = cfg_rx.borrow().clone();
                app.duck_pct.store(next.spotify.duck, Ordering::Relaxed);
                if (next.spotify.enabled, device_name(&next)) != key {
                    break;
                }
            }
        };
        tokio::select! {
            r = device(&app, &name, rx) => {
                if let Err(e) = r {
                    app.log(format!("Spotify: {e:#}"));
                    app.status.lock().unwrap().spotify.error = format!("{e:#}");
                }
                *app.music.lock().unwrap() = None;
                tokio::time::sleep(Duration::from_secs(15)).await;
            }
            _ = restart => {
                app.log("Spotify-Einstellungen geändert – starte Spotify neu");
            }
        }
        *app.music.lock().unwrap() = None;
        app.playback.lock().unwrap().music.clear();
    }
}

fn set(app: &App, f: impl FnOnce(&mut Status)) {
    f(&mut app.status.lock().unwrap().spotify);
}

/// Ein Spotify-Gerät: Anmeldung (gespeichert oder über die App), Wiedergabe, Befehle. Endet nur bei einem Fehler.
async fn device(
    app: &Arc<App>,
    name: &str,
    mut ctrl: mpsc::UnboundedReceiver<Ctrl>,
) -> anyhow::Result<()> {
    let dir = app.data.join("spotify");
    std::fs::create_dir_all(&dir)?;
    // Keine Lieder auf der SD-Karte zwischenspeichern, nur Anmeldung und Lautstärke
    let cache = Cache::new(Some(&dir), Some(&dir), None, None)?;
    let session_config = SessionConfig {
        device_id: device_id(name),
        ..SessionConfig::default()
    };
    let connect_config = ConnectConfig {
        name: name.to_string(),
        device_type: DeviceType::Speaker,
        ..ConnectConfig::default()
    };

    let mut discovery = match librespot::discovery::Discovery::builder(
        session_config.device_id.clone(),
        session_config.client_id.clone(),
    )
    .name(name.to_string())
    .device_type(DeviceType::Speaker)
    .port(PORT)
    .launch()
    {
        Ok(d) => {
            set(app, |s| s.discovery = true);
            Some(d)
        }
        Err(e) => {
            app.log(format!(
                "Spotify: in der App nicht findbar ({e}) – läuft avahi-daemon auf dem Pi?"
            ));
            None
        }
    };
    let mut credentials: Option<Credentials> = cache.credentials();
    if credentials.is_none() && discovery.is_none() {
        anyhow::bail!("keine Anmeldung gespeichert und in der App nicht findbar");
    }
    if credentials.is_none() {
        app.log(format!(
            "Spotify: in der Spotify-App einmal „{name}“ als Gerät auswählen"
        ));
    }

    let mixer = mixer::find(None).ok_or_else(|| anyhow::anyhow!("kein Spotify-Mixer"))?(
        MixerConfig::default(),
    )?;
    let mut session = Session::new(session_config.clone(), Some(cache.clone()));
    let sink_app = app.clone();
    let player = Player::new(
        PlayerConfig::default(),
        session.clone(),
        mixer.get_soft_volume(),
        move || Box::new(MusicSink::new(sink_app)) as Box<dyn Sink>,
    );
    let mut events = player.get_player_event_channel();

    let mut spirc: Option<Spirc> = None;
    let mut spirc_task: Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>> = None;
    let mut connecting = credentials.is_some();
    let mut restarts: Vec<Instant> = Vec::new();

    loop {
        tokio::select! {
            c = async { discovery.as_mut().expect("discovery").next().await }, if discovery.is_some() => {
                let Some(c) = c else {
                    set(app, |s| s.discovery = false);
                    discovery = None;
                    continue;
                };
                app.log("Spotify: in der App ausgewählt – melde an");
                credentials = Some(c);
                restarts.clear();
                if let Some(s) = spirc.take() {
                    let _ = s.shutdown();
                }
                if let Some(t) = spirc_task.take() {
                    tokio::task::spawn_local(t);
                }
                if !session.is_invalid() {
                    session.shutdown();
                }
                connecting = true;
            }
            _ = async {}, if connecting => {
                connecting = false;
                if session.is_invalid() {
                    session = Session::new(session_config.clone(), Some(cache.clone()));
                    player.set_session(session.clone());
                }
                let c = credentials.clone().unwrap_or_default();
                match Spirc::new(connect_config.clone(), session.clone(), c, player.clone(), mixer.clone()).await {
                    Ok((s, task)) => {
                        let user = session.username();
                        app.log(format!("Spotify: angemeldet als {user} – „{name}“ ist in der App da"));
                        set(app, |st| { st.connected = true; st.user = user; st.error.clear(); });
                        spirc = Some(s);
                        spirc_task = Some(Box::pin(task));
                    }
                    Err(e) => {
                        set(app, |st| { st.connected = false; st.error = format!("Anmeldung fehlgeschlagen: {e}"); });
                        app.log(format!("Spotify: Anmeldung fehlgeschlagen ({e}) – in der App neu auswählen"));
                    }
                }
            }
            _ = async { spirc_task.as_mut().expect("spirc").await }, if spirc_task.is_some() => {
                spirc_task = None;
                spirc = None;
                set(app, |s| { s.connected = false; s.playing = false; });
                restarts.retain(|t| t.elapsed() < Duration::from_secs(600));
                if restarts.len() >= 5 {
                    anyhow::bail!("Verbindung zu Spotify bricht ständig ab");
                }
                restarts.push(Instant::now());
                app.log("Spotify: Verbindung weg – verbinde neu");
                if !session.is_invalid() {
                    session.shutdown();
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
                connecting = credentials.is_some();
            }
            Some(ev) = events.recv() => on_event(app, ev),
            Some(c) = ctrl.recv() => {
                let Some(s) = spirc.as_ref() else {
                    app.log(format!("Spotify: noch nicht angemeldet – in der Spotify-App einmal „{name}“ auswählen"));
                    continue;
                };
                if let Err(e) = command(app, s, c) {
                    app.log(format!("Spotify: {e}"));
                }
            }
        }
    }
}

fn command(app: &App, s: &Spirc, c: Ctrl) -> Result<(), librespot::core::Error> {
    match c {
        Ctrl::Play => {
            s.activate()?;
            s.play()
        }
        Ctrl::Pause => s.pause(),
        Ctrl::Toggle => s.play_pause(),
        Ctrl::Next => s.next(),
        Ctrl::Prev => s.prev(),
        Ctrl::Volume(p) => s.set_volume(pct_to_volume(p)),
        Ctrl::VolumeBy(d) => {
            let now = app.status.lock().unwrap().spotify.volume as i32;
            s.set_volume(pct_to_volume((now + d).clamp(0, 100) as u32))
        }
        Ctrl::Load(uri) => {
            s.activate()?;
            let options = LoadRequestOptions {
                start_playing: true,
                ..Default::default()
            };
            let req = if uri.starts_with("spotify:track:") || uri.starts_with("spotify:episode:") {
                LoadRequest::from_tracks(vec![uri], options)
            } else {
                LoadRequest::from_context_uri(uri, options)
            };
            s.load(req)
        }
    }
}

pub fn pct_to_volume(p: u32) -> u16 {
    (u32::from(u16::MAX) * p.min(100) / 100) as u16
}

pub fn volume_to_pct(v: u16) -> u32 {
    (u32::from(v) * 100 + u32::from(u16::MAX) / 2) / u32::from(u16::MAX)
}

fn on_event(app: &App, ev: PlayerEvent) {
    match ev {
        PlayerEvent::Playing { .. } => set(app, |s| s.playing = true),
        PlayerEvent::Paused { .. } | PlayerEvent::Stopped { .. } => set(app, |s| s.playing = false),
        PlayerEvent::VolumeChanged { volume } => set(app, |s| s.volume = volume_to_pct(volume)),
        PlayerEvent::TrackChanged { audio_item } => {
            let artist = match &audio_item.unique_fields {
                librespot::metadata::audio::UniqueFields::Track { artists, .. } => artists
                    .0
                    .iter()
                    .map(|a| a.name.clone())
                    .collect::<Vec<_>>()
                    .join(", "),
                librespot::metadata::audio::UniqueFields::Episode { show_name, .. } => {
                    show_name.clone()
                }
                #[allow(unreachable_patterns)]
                _ => String::new(),
            };
            set(app, |s| {
                s.track = audio_item.name.clone();
                s.artist = artist;
            });
        }
        PlayerEvent::SessionDisconnected { .. } => set(app, |s| {
            s.connected = false;
            s.playing = false;
        }),
        _ => {}
    }
}

/// Nimmt die Musik von librespot (44,1 kHz Stereo) und legt sie als Mono in Geräterate in `Playback::music`.
/// Blockiert, solange genug Musik bereitliegt – so gibt der Lautsprecher das Tempo vor, nicht der Decoder.
struct MusicSink {
    app: Arc<App>,
    rs: Resampler,
    rate: u32,
    mono: Vec<f32>,
    out: Vec<f32>,
}

impl MusicSink {
    fn new(app: Arc<App>) -> Self {
        let rate = app.output_rate().max(8000);
        Self {
            app,
            rs: Resampler::new(SPOTIFY_RATE, rate),
            rate,
            mono: Vec::new(),
            out: Vec::new(),
        }
    }
}

impl Sink for MusicSink {
    fn stop(&mut self) -> SinkResult<()> {
        // Pause soll sofort still sein, nicht erst nach dem Puffer
        self.app.playback.lock().unwrap().music.clear();
        Ok(())
    }

    fn write(&mut self, packet: AudioPacket, _: &mut Converter) -> SinkResult<()> {
        let AudioPacket::Samples(samples) = packet else {
            return Ok(());
        };
        let rate = self.app.output_rate();
        if rate == 0 {
            // Audio läuft gerade nicht: im Takt weiterlaufen, aber nichts ablegen
            let secs = samples.len() as f32 / 2.0 / SPOTIFY_RATE as f32;
            std::thread::sleep(Duration::from_secs_f32(secs));
            return Ok(());
        }
        if rate != self.rate {
            self.rs = Resampler::new(SPOTIFY_RATE, rate);
            self.rate = rate;
        }
        self.mono.clear();
        self.mono.extend(
            samples
                .chunks(2)
                .map(|f| (f.iter().sum::<f64>() / f.len() as f64) as f32),
        );
        self.out.clear();
        self.rs.process(&self.mono, &mut self.out);
        let ahead = (rate as f32 * AHEAD_S) as usize;
        let waited = Instant::now();
        loop {
            let len = self.app.playback.lock().unwrap().music.len();
            // Hängt der Lautsprecher (Gerät weg), nicht ewig warten
            if len < ahead || waited.elapsed() > Duration::from_secs(2) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut p = self.app.playback.lock().unwrap();
        if p.music.len() > ahead * 4 {
            p.music.clear();
        }
        p.music.extend(self.out.iter().copied());
        Ok(())
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
    fn feste_geraete_id() {
        let a = device_id("Nina Küche");
        assert_eq!(a.len(), 40);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(a, device_id("nina küche"));
        assert_ne!(a, device_id("Stella"));
    }

    #[test]
    fn lautstaerke_umrechnen() {
        assert_eq!(pct_to_volume(100), u16::MAX);
        assert_eq!(pct_to_volume(0), 0);
        assert_eq!(volume_to_pct(pct_to_volume(37)), 37);
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
