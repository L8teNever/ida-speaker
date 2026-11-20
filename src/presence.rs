//! Anwesenheit: Das Wake-Word gilt nur, wenn das gekoppelte Handy in der Nähe ist.
//! Der Pi klopft regelmäßig per Bluetooth an; antwortet das Handy eine Weile nicht, ist es „weg“.

use crate::bluetooth::{self, Reach};
use crate::state::{App, Phase, now_s};
use serde::Serialize;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Default)]
pub struct Status {
    pub enabled: bool,
    pub name: String,
    pub present: bool,
    /// Sekunden seit Epoche, zuletzt gesehen (0 = nie).
    pub last_seen: u64,
    pub rssi: Option<i32>,
    pub detail: String,
}

/// Ist das Handy da? `last_seen` ist der letzte Kontakt, `rssi` die letzte bekannte Signalstärke.
pub fn decide(
    now: u64,
    last_seen: u64,
    away_after_s: u32,
    rssi: Option<i32>,
    min_rssi: i32,
) -> bool {
    if last_seen == 0 || now.saturating_sub(last_seen) > away_after_s as u64 {
        return false;
    }
    match rssi {
        Some(r) if min_rssi < 0 => r >= min_rssi,
        _ => true,
    }
}

pub async fn run(app: Arc<App>) {
    let mut last_seen = 0u64;
    let mut last_rssi: Option<i32> = None;
    let mut was: Option<bool> = None;
    loop {
        let cfg = app.cfg().presence;
        if !cfg.enabled || cfg.mac.is_empty() {
            app.present.store(true, Ordering::Relaxed);
            if app.phase() == Phase::Away {
                app.set_phase(app.idle_phase());
            }
            app.status.lock().unwrap().presence = Status {
                enabled: false,
                ..Default::default()
            };
            was = None;
            last_seen = 0;
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        // Während eines Gesprächs nicht funken (Bluetooth-Box) – wer redet, ist da
        let talking = app.talking.load(Ordering::Relaxed);
        let (reach, rssi) = if talking {
            (Reach::Near, last_rssi)
        } else {
            bluetooth::probe(&cfg.mac).await
        };
        let now = now_s();
        if reach == Reach::Near {
            last_seen = now;
        }
        if rssi.is_some() {
            last_rssi = rssi;
        }
        let present = if reach == Reach::Unknown && last_seen == 0 {
            true // Bluetooth des Pi geht nicht: lieber hören als taub sein
        } else {
            decide(now, last_seen, cfg.away_after_s, last_rssi, cfg.min_rssi)
        };
        app.present.store(present, Ordering::Relaxed);
        if was != Some(present) {
            app.log(if present {
                format!("„{}“ ist da – höre auf das Wake-Word", cfg.name)
            } else {
                format!(
                    "„{}“ ist weg – Wake-Word aus, bis es wieder da ist",
                    cfg.name
                )
            });
            let phase = app.phase();
            if matches!(phase, Phase::Listening | Phase::Away) && !app.muted() {
                app.set_phase(if present {
                    Phase::Listening
                } else {
                    Phase::Away
                });
            }
            was = Some(present);
        }
        app.status.lock().unwrap().presence = Status {
            enabled: true,
            name: cfg.name.clone(),
            present,
            last_seen,
            rssi: last_rssi,
            detail: match reach {
                Reach::Near => "antwortet".into(),
                Reach::Away => "antwortet nicht".into(),
                Reach::Unknown => "Bluetooth des Pi nicht erreichbar".into(),
            },
        };
        // Da: seltener prüfen (schont den Akku), weg: öfter, damit es schnell wieder reagiert
        tokio::time::sleep(Duration::from_secs(if present { 20 } else { 8 })).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_decision() {
        assert!(!decide(1000, 0, 60, None, 0)); // nie gesehen
        assert!(decide(1000, 990, 60, None, 0));
        assert!(!decide(1000, 900, 60, None, 0)); // zu lange her
        assert!(decide(1000, 990, 60, Some(-70), -75));
        assert!(!decide(1000, 990, 60, Some(-80), -75)); // zu schwach = zu weit
        assert!(decide(1000, 990, 60, Some(-90), 0)); // Grenze aus
    }
}
