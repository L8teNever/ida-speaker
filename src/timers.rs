//! Timer und Wecker: laufen auf dem Lautsprecher selbst (klingeln auch ohne Internet), gespeichert in `/data/timers.json`.
//! Gestellt werden sie im Gespräch (Server schickt `{"type":"timer"}`) oder gelöscht auf der Website.
//! Klingeln endet mit dem Wake-Word, auf der Website oder nach 2 Minuten.

use crate::audio::{self, Beep};
use crate::state::App;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Timer {
    pub id: u64,
    pub label: String,
    /// Fällig (Unix-Zeit, Sekunden).
    pub due: u64,
    /// Wecker (Uhrzeit) statt Timer (Dauer).
    #[serde(default)]
    pub alarm: bool,
}

static TIMERS: Mutex<Vec<Timer>> = Mutex::new(Vec::new());
static RINGING: AtomicBool = AtomicBool::new(false);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
/// Wie lange es höchstens klingelt.
const RING_MAX: Duration = Duration::from_secs(120);

pub fn now_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn save(app: &App, list: &[Timer]) {
    let path = app.data.join("timers.json");
    let tmp = app.data.join("timers.json.tmp");
    let ok = serde_json::to_vec(list)
        .map_err(std::io::Error::other)
        .and_then(|b| std::fs::write(&tmp, b))
        .and_then(|_| std::fs::rename(&tmp, &path));
    if let Err(e) = ok {
        app.log(format!("Timer: nicht gespeichert ({e})"));
    }
}

/// Gespeicherte Timer laden und jede Sekunde prüfen, ob einer fällig ist.
pub fn spawn(app: Arc<App>) {
    if let Ok(b) = std::fs::read(app.data.join("timers.json"))
        && let Ok(list) = serde_json::from_slice::<Vec<Timer>>(&b)
    {
        let next = list.iter().map(|t| t.id).max().unwrap_or(0) + 1;
        NEXT_ID.store(next, Ordering::Relaxed);
        *TIMERS.lock().unwrap() = list;
    }
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            let now = now_s();
            let due: Vec<Timer> = {
                let mut list = TIMERS.lock().unwrap();
                let (due, rest): (Vec<_>, Vec<_>) = list.drain(..).partition(|t| t.due <= now);
                *list = rest;
                if !due.is_empty() {
                    save(&app, &list);
                }
                due
            };
            for t in due {
                // Mehr als 10 Minuten verpasst (Pi war aus): nicht mehr klingeln
                if now.saturating_sub(t.due) > 600 {
                    app.log(format!("Timer „{}“ verpasst", t.label));
                    continue;
                }
                app.log(format!("{} „{}“ klingelt", kind(&t), t.label));
                ring(app.clone());
            }
        }
    });
}

fn kind(t: &Timer) -> &'static str {
    if t.alarm { "Wecker" } else { "Timer" }
}

/// Klingelt, bis es gestoppt wird oder RING_MAX vorbei ist (eigener Thread, blockiert nichts).
fn ring(app: Arc<App>) {
    if RINGING.swap(true, Ordering::SeqCst) {
        return; // klingelt schon
    }
    std::thread::spawn(move || {
        let start = Instant::now();
        while RINGING.load(Ordering::SeqCst) && start.elapsed() < RING_MAX {
            if !app.playing() {
                audio::beep(&app, Beep::Alarm);
            }
            std::thread::sleep(Duration::from_millis(1600));
        }
        RINGING.store(false, Ordering::SeqCst);
    });
}

pub fn ringing() -> bool {
    RINGING.load(Ordering::SeqCst)
}

/// Klingeln beenden. `true` = es hat geklingelt.
pub fn stop(app: &App) -> bool {
    let was = RINGING.swap(false, Ordering::SeqCst);
    if was {
        app.clear_playback();
        app.log("Klingeln aus");
    }
    was
}

pub fn list() -> Vec<Timer> {
    let mut l = TIMERS.lock().unwrap().clone();
    l.sort_by_key(|t| t.due);
    l
}

pub fn set(app: &App, label: &str, due: u64, alarm: bool) {
    let label: String = label.chars().filter(|c| !c.is_control()).take(40).collect();
    let label = if label.trim().is_empty() {
        if alarm { "Wecker" } else { "Timer" }.to_string()
    } else {
        label.trim().to_string()
    };
    let t = Timer {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        label,
        due,
        alarm,
    };
    app.log(format!(
        "{} „{}“ gestellt (in {} s)",
        kind(&t),
        t.label,
        due.saturating_sub(now_s())
    ));
    let mut list = TIMERS.lock().unwrap();
    list.push(t);
    list.truncate(30);
    save(app, &list);
}

/// Löscht Timer mit diesem Namen (Groß/Klein egal), leer = alle. Ergebnis: wie viele.
pub fn cancel(app: &App, label: &str) -> usize {
    let want = label.trim().to_lowercase();
    let mut list = TIMERS.lock().unwrap();
    let before = list.len();
    list.retain(|t| !want.is_empty() && !t.label.to_lowercase().contains(&want));
    let n = before - list.len();
    save(app, &list);
    n
}

pub fn cancel_id(app: &App, id: u64) -> bool {
    let mut list = TIMERS.lock().unwrap();
    let before = list.len();
    list.retain(|t| t.id != id);
    let gone = list.len() != before;
    save(app, &list);
    gone
}
