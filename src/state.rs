//! Gemeinsamer Zustand zwischen Audio-Threads, Gespräch, WARP und Website.

use crate::config::Config;
use serde::Serialize;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, watch};

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Audio startet oder Modell fehlt.
    Starting,
    /// Wartet auf das Wake-Word.
    Listening,
    /// Ticket holen und verbinden.
    Connecting,
    /// Gespräch läuft.
    Talking,
    /// Stummgeschaltet: hört nicht auf das Wake-Word.
    Muted,
    /// Audio-Gerät fehlt oder ist kaputt.
    Error,
    /// Mein Handy ist nicht in der Nähe: hört nicht auf das Wake-Word.
    Away,
}

#[derive(Debug, Clone, Serialize)]
pub struct Trigger {
    pub at: u64,
    pub source: String,
    pub result: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct WarpStatus {
    pub installed: bool,
    pub running: bool,
    pub connected: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct AudioInfo {
    pub input: String,
    pub output: String,
    pub input_rate: u32,
    pub output_rate: u32,
    pub echo_filter: bool,
    pub model: bool,
    pub unknown_words: Vec<String>,
    pub heard: String,
    pub error: String,
}

pub enum Command {
    /// Gespräch starten (Wake-Word oder Knopf auf der Website).
    Start(String),
    /// Laufendes Gespräch beenden.
    Stop,
}

pub struct Status {
    pub phase: Phase,
    pub triggers: VecDeque<Trigger>,
    pub logs: VecDeque<String>,
    pub warp: WarpStatus,
    pub audio: AudioInfo,
    pub last_text: VecDeque<(String, String)>,
    pub wifi: crate::wifi::Status,
    pub bluetooth: crate::bluetooth::Status,
    pub presence: crate::presence::Status,
}

pub struct App {
    pub data: PathBuf,
    pub config: watch::Sender<Config>,
    pub status: Mutex<Status>,
    pub sessions: Mutex<std::collections::HashMap<String, u64>>,
    pub commands: mpsc::UnboundedSender<Command>,
    /// Mikrofon-Pegel (RMS nach dem Filter) für die Website.
    pub level: AtomicU32,
    pub muted: AtomicBool,
    /// Letzter angemeldeter Seitenaufruf (Sekunden), damit der WLAN-Wächter den Hotspot nicht unter dir wegzieht.
    pub last_web: AtomicU64,
    /// Audio neu aufbauen (z. B. Bluetooth-Lautsprecher kam oder ging).
    pub rebuild_audio: AtomicBool,
    /// Handy in der Nähe (oder Anwesenheit aus): nur dann gilt das Wake-Word.
    pub present: AtomicBool,
    /// Gespräch läuft: Mikrofon geht an IDA statt ans Wake-Word.
    pub talking: AtomicBool,
    /// Mikrofon-Audio für das laufende Gespräch (16 kHz, i16).
    pub mic: Mutex<Option<mpsc::Sender<Vec<i16>>>>,
    /// Was der Lautsprecher abspielen soll (mono, Geräterate).
    pub playback: Mutex<Playback>,
}

#[derive(Default)]
pub struct Playback {
    pub queue: VecDeque<f32>,
    pub rate: u32,
    /// Sekunden seit Epoche, bis zu denen „Stella spricht“ gilt (für Notfall-Modus und Stille-Erkennung).
    pub busy_until_ms: u64,
}

pub fn now_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl App {
    pub fn new(
        data: PathBuf,
        config: Config,
        commands: mpsc::UnboundedSender<Command>,
    ) -> Arc<Self> {
        Arc::new(Self {
            data,
            config: watch::channel(config).0,
            status: Mutex::new(Status {
                phase: Phase::Starting,
                triggers: VecDeque::new(),
                logs: VecDeque::new(),
                warp: WarpStatus::default(),
                audio: AudioInfo::default(),
                last_text: VecDeque::new(),
                wifi: Default::default(),
                bluetooth: Default::default(),
                presence: Default::default(),
            }),
            sessions: Mutex::new(Default::default()),
            commands,
            level: AtomicU32::new(0),
            muted: AtomicBool::new(false),
            last_web: AtomicU64::new(0),
            rebuild_audio: AtomicBool::new(false),
            present: AtomicBool::new(true),
            talking: AtomicBool::new(false),
            mic: Mutex::new(None),
            playback: Mutex::new(Playback::default()),
        })
    }

    pub fn cfg(&self) -> Config {
        self.config.borrow().clone()
    }

    pub fn save_config(&self, next: Config) -> anyhow::Result<()> {
        next.save(&self.data.join("config.toml"))?;
        self.config.send_replace(next);
        Ok(())
    }

    pub fn log(&self, msg: impl Into<String>) {
        let msg = msg.into();
        tracing::info!("{msg}");
        let line = format!("{} {msg}", clock());
        let mut s = self.status.lock().unwrap();
        s.logs.push_back(line);
        while s.logs.len() > 200 {
            s.logs.pop_front();
        }
    }

    pub fn set_phase(&self, phase: Phase) {
        self.status.lock().unwrap().phase = phase;
    }

    pub fn phase(&self) -> Phase {
        self.status.lock().unwrap().phase
    }

    pub fn trigger(&self, source: &str, result: &str) {
        let mut s = self.status.lock().unwrap();
        s.triggers.push_front(Trigger {
            at: now_s(),
            source: source.into(),
            result: result.into(),
        });
        s.triggers.truncate(20);
    }

    pub fn said(&self, who: &str, text: &str) {
        let mut s = self.status.lock().unwrap();
        match s.last_text.back_mut() {
            Some((w, t)) if w == who => t.push_str(text),
            _ => s.last_text.push_back((who.into(), text.into())),
        }
        while s.last_text.len() > 30 {
            s.last_text.pop_front();
        }
    }

    /// Spielt Audio ab (mono, beliebige Rate wird vorher umgerechnet).
    pub fn play(&self, samples: &[f32]) {
        let mut p = self.playback.lock().unwrap();
        p.queue.extend(samples.iter().copied());
        let rate = p.rate.max(1) as u64;
        p.busy_until_ms = now_ms() + p.queue.len() as u64 * 1000 / rate + 300;
    }

    pub fn clear_playback(&self) {
        let mut p = self.playback.lock().unwrap();
        p.queue.clear();
        p.busy_until_ms = now_ms();
    }

    pub fn playing(&self) -> bool {
        let p = self.playback.lock().unwrap();
        !p.queue.is_empty() || p.busy_until_ms > now_ms()
    }

    pub fn output_rate(&self) -> u32 {
        self.playback.lock().unwrap().rate
    }

    pub fn web_active(&self) -> bool {
        now_s().saturating_sub(self.last_web.load(Ordering::Relaxed)) < 120
    }

    /// Zustand, wenn gerade kein Gespräch läuft.
    pub fn idle_phase(&self) -> Phase {
        if self.muted() {
            Phase::Muted
        } else if !self.present.load(Ordering::Relaxed) {
            Phase::Away
        } else {
            Phase::Listening
        }
    }

    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }
}

fn clock() -> String {
    let s = now_s();
    let (h, m, sec) = ((s / 3600) % 24, (s / 60) % 60, s % 60);
    format!("{h:02}:{m:02}:{sec:02}Z")
}
