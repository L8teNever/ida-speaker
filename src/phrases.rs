//! Ansagen für Fehler („Ich erreiche den Server gerade nicht …“), mit Ninas Stimme vom Server geholt und in
//! `/data/phrases` gespeichert. So kann der Lautsprecher sie auch ohne Internet selbst abspielen.

use crate::audio::VoiceOut;
use crate::session::{access_headers, http};
use crate::state::App;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Deserialize)]
struct Meta {
    voice: String,
    ids: Vec<String>,
}

fn dir(app: &App) -> PathBuf {
    app.data.join("phrases")
}

/// Holt die Ansagen kurz nach dem Start und danach alle 6 Stunden (neue Stimme = neue Ansagen).
pub fn spawn(app: Arc<App>) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(20)).await;
        loop {
            let wait = match fetch(&app).await {
                Ok(0) => 6 * 3600,
                Ok(n) => {
                    app.log(format!("Ansagen: {n} neu geladen"));
                    6 * 3600
                }
                Err(e) => {
                    tracing::debug!("Ansagen: {e:#}");
                    600
                }
            };
            tokio::time::sleep(Duration::from_secs(wait)).await;
        }
    });
}

/// Lädt fehlende oder veraltete Ansagen. Ergebnis: wie viele neu sind.
async fn fetch(app: &App) -> anyhow::Result<usize> {
    let cfg = app.cfg();
    let ida = &cfg.ida;
    if ida.url.is_empty() || ida.device_name.is_empty() || ida.device_key.is_empty() {
        anyhow::bail!("IDA nicht eingerichtet");
    }
    let post = |path: String| {
        http()
            .post(format!("{}{path}", ida.url))
            .header("x-device-name", &ida.device_name)
            .header("x-device-key", &ida.device_key)
            .headers(access_headers(&cfg))
            .timeout(Duration::from_secs(60))
    };
    let meta: Meta = post("/device/phrases".into())
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let d = dir(app);
    std::fs::create_dir_all(&d)?;
    let same_voice =
        std::fs::read_to_string(d.join("voice")).ok().as_deref() == Some(meta.voice.as_str());
    let mut new = 0;
    for id in meta.ids.iter().filter(|id| safe_id(id)) {
        let file = d.join(format!("{id}.pcm"));
        if same_voice && file.exists() {
            continue;
        }
        let bytes = post(format!("/device/phrase/{id}"))
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        let tmp = d.join(format!("{id}.pcm.tmp"));
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &file)?;
        new += 1;
    }
    std::fs::write(d.join("voice"), &meta.voice)?;
    Ok(new)
}

fn safe_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 32 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Spielt eine gespeicherte Ansage ab. `false` = gibt es (noch) nicht.
pub fn play(app: &App, id: &str) -> bool {
    if !safe_id(id) {
        return false;
    }
    let Ok(pcm) = std::fs::read(dir(app).join(format!("{id}.pcm"))) else {
        return false;
    };
    if pcm.is_empty() || app.output_rate() == 0 {
        return false;
    }
    VoiceOut::new(app.output_rate()).push(app, &pcm);
    true
}

/// Welche Ansage zu einem Fehler passt.
pub fn for_error(err: &str) -> &'static str {
    let e = err.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| e.contains(w));
    if has(&[
        "nicht eingerichtet",
        "service-token",
        "access verlangt",
        "access blockt",
    ]) {
        "setup"
    } else if has(&["in zwei gesprächen", "besetzt"]) {
        "busy"
    } else if has(&[
        "nicht erreichbar",
        "error sending request",
        "connection refused",
        "connection reset",
        "dns",
        "timed out",
        "timeout",
        "deadline",
        "network is unreachable",
        "no route to host",
        "failed to lookup",
        "elapsed",
    ]) {
        "offline"
    } else {
        "error"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passende_ansage() {
        assert_eq!(
            for_error("IDA nicht erreichbar: error sending request"),
            "offline"
        );
        assert_eq!(for_error("deadline has elapsed"), "offline");
        assert_eq!(for_error("Stella ist gerade in zwei Gesprächen"), "busy");
        assert_eq!(
            for_error("IDA ist noch nicht eingerichtet (Adresse, Gerätename, Schlüssel)"),
            "setup"
        );
        assert_eq!(for_error("Gemini ist kaputt"), "error");
        assert!(safe_id("offline") && !safe_id("../x") && !safe_id(""));
    }
}
