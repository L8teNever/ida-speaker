//! Ein Gespräch mit IDA: Ticket über den Geräte-Schlüssel holen, WebSocket `/live?mode=speaker` öffnen,
//! Mikrofon hochschicken (PCM 16 kHz) und Stellas Stimme abspielen (PCM 24 kHz) – dasselbe Protokoll wie die Live-App.

use crate::audio::{self, Beep, VoiceOut};
use crate::config::Config;
use crate::state::{App, Command, Phase, now_ms};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

#[derive(Debug, Deserialize)]
pub struct Ticket {
    pub ticket: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub busy: bool,
}

pub fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()
        .expect("http client")
}

/// Holt ein Ticket (gilt eine Minute). Auch der Test-Knopf auf der Website nutzt das.
pub async fn ticket(cfg: &Config) -> anyhow::Result<Ticket> {
    ticket_for(cfg, false).await
}

/// Was beim Ticket mitgeht: Anweisungen, Smart Home und beim zweiten Namen die Befehls-Persönlichkeit.
pub fn ticket_body(cfg: &Config, quick: bool) -> serde_json::Value {
    if !quick {
        return serde_json::json!({ "prompt": cfg.assistant.prompt, "smart_home": cfg.assistant.smart_home });
    }
    let q = &cfg.quick;
    let prompt = [cfg.assistant.prompt.trim(), q.prompt.trim()]
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let name = if q.name.is_empty() {
        capitalize(&cfg.wake.second_word)
    } else {
        q.name.clone()
    };
    serde_json::json!({ "prompt": prompt, "smart_home": q.smart_home, "name": name, "voice": q.voice, "oneshot": q.oneshot })
}

fn capitalize(s: &str) -> String {
    s.split_whitespace()
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_uppercase().chain(c).collect::<String>())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

async fn ticket_for(cfg: &Config, quick: bool) -> anyhow::Result<Ticket> {
    let ida = &cfg.ida;
    if ida.url.is_empty() || ida.device_name.is_empty() || ida.device_key.is_empty() {
        anyhow::bail!("IDA ist noch nicht eingerichtet (Adresse, Gerätename, Schlüssel)");
    }
    let res = http()
        .post(format!("{}/device/ticket", ida.url))
        .header("x-device-name", &ida.device_name)
        .header("x-device-key", &ida.device_key)
        .headers(access_headers(cfg))
        .json(&ticket_body(cfg, quick))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("IDA nicht erreichbar: {e}"))?;
    let json = res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("json"));
    let to_access = res
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("cloudflareaccess.com"));
    match res.status().as_u16() {
        200 if json => Ok(res.json().await?),
        _ if to_access => anyhow::bail!(
            "Cloudflare Access verlangt eine Anmeldung – Service-Token eintragen (Einrichtung → Verbindung) oder in Access freigeben"
        ),
        401 | 403 if !json => anyhow::bail!(
            "Cloudflare Access blockt – der Service-Token ist falsch oder an der Voice-App nicht als „Service Auth“ freigegeben"
        ),
        403 => anyhow::bail!(
            "IDA lehnt den Lautsprecher ab – Name oder Schlüssel falsch, oder nicht in VOICE_DEVICES"
        ),
        404 => anyhow::bail!(
            "Unter dieser Adresse läuft kein IDA-Voice-Dienst mit Lautsprecher-Unterstützung"
        ),
        200 => {
            anyhow::bail!("Unter dieser Adresse antwortet etwas anderes als der IDA-Voice-Dienst")
        }
        s => anyhow::bail!("IDA antwortet mit HTTP {s}"),
    }
}

/// Cloudflare Access vor der Voice-Domain: Service-Token als Header (bei Ticket und WebSocket).
pub fn access_headers(cfg: &Config) -> reqwest::header::HeaderMap {
    let mut h = reqwest::header::HeaderMap::new();
    let (id, secret) = (&cfg.ida.access_client_id, &cfg.ida.access_client_secret);
    if !id.is_empty()
        && !secret.is_empty()
        && let (Ok(i), Ok(s)) = (id.parse(), secret.parse())
    {
        h.insert("cf-access-client-id", i);
        h.insert("cf-access-client-secret", s);
    }
    h
}

pub fn ws_url(base: &str, ticket: &str) -> String {
    let base = base.trim_end_matches('/');
    let ws = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_string()
    };
    format!("{ws}/live?ticket={ticket}&mode=speaker")
}

fn ws_request(
    cfg: &Config,
    ticket: &str,
) -> anyhow::Result<tokio_tungstenite::tungstenite::handshake::client::Request> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = ws_url(&cfg.ida.url, ticket).into_client_request()?;
    for (k, v) in access_headers(cfg).iter() {
        req.headers_mut().insert(k.clone(), v.clone());
    }
    Ok(req)
}

#[derive(Debug, Deserialize)]
struct ServerMsg {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    who: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    running: bool,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    message: String,
}

/// Nimmt Befehle (Wake-Word, Knopf) entgegen und führt immer höchstens ein Gespräch.
pub async fn run(app: Arc<App>, mut commands: mpsc::UnboundedReceiver<Command>) {
    while let Some(cmd) = commands.recv().await {
        let Command::Start { source, quick, pre } = cmd else {
            continue;
        };
        let result = match talk(&app, &mut commands, quick, pre).await {
            Ok(r) => r,
            Err(e) => {
                app.log(format!("Gespräch: {e:#}"));
                if app.cfg().audio.tones {
                    audio::beep(&app, Beep::Error);
                }
                format!("Fehler: {e:#}")
            }
        };
        app.trigger(&source, &result);
        app.talking.store(false, Ordering::Relaxed);
        *app.mic.lock().unwrap() = None;
        app.set_phase(app.idle_phase());
        // Befehle, die während des Gesprächs auflaufen (z. B. doppelt erkanntes Wake-Word), verwerfen
        while commands.try_recv().is_ok() {}
    }
}

async fn talk(
    app: &Arc<App>,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    quick: bool,
    pre: Vec<i16>,
) -> anyhow::Result<String> {
    let cfg = app.cfg();
    app.set_phase(Phase::Connecting);
    if cfg.audio.tones {
        audio::beep(app, Beep::Listen);
    }
    // Mikrofon schon beim Verbinden mitschneiden (bis 8 s): beim Befehl („Computer, Licht an“) zählt jedes Wort
    let (mic_tx, mut mic_rx) = mpsc::channel::<Vec<i16>>(800);
    if quick {
        *app.mic.lock().unwrap() = Some(mic_tx.clone());
        app.talking.store(true, Ordering::Relaxed);
    }
    let t = ticket_for(&cfg, quick).await?;
    if t.busy {
        anyhow::bail!(
            "{} ist gerade in zwei Gesprächen",
            if t.name.is_empty() { "IDA" } else { &t.name }
        );
    }
    let (ws, _) = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::connect_async(ws_request(&cfg, &t.ticket)?),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Verbindung zu IDA dauert zu lange"))??;
    let (mut tx, mut rx) = ws.split();

    *app.mic.lock().unwrap() = Some(mic_tx);
    app.talking.store(true, Ordering::Relaxed);
    app.set_phase(Phase::Talking);
    app.status.lock().unwrap().last_text.clear();
    app.log("Gespräch verbunden");

    let started = Instant::now();
    let max = Duration::from_secs(cfg.call.max_minutes as u64 * 60);
    let mut silence = Duration::from_secs(cfg.call.silence_end_s as u64);
    if quick && cfg.quick.oneshot {
        silence = silence.min(Duration::from_secs(6));
    }
    // Bis IDA „ready“ meldet, sammeln: zuerst das, was nach dem Namen schon gesagt wurde
    let mut ready = !quick;
    let mut held: Vec<u8> = pre.iter().flat_map(|s| s.to_le_bytes()).collect();
    let mut last_activity = Instant::now();
    let mut voice = VoiceOut::new(app.output_rate());
    let mut ending: Option<Instant> = None;
    let mut outcome = String::from("beendet");
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut batch: Vec<u8> = Vec::with_capacity(3200);

    loop {
        tokio::select! {
            Some(pcm) = mic_rx.recv() => {
                if crate::dsp::rms_i16(&pcm) >= cfg.call.speech_rms {
                    last_activity = Instant::now();
                }
                if !ready {
                    held.extend(pcm.iter().flat_map(|s| s.to_le_bytes()));
                } else if ending.is_none() {
                    batch.extend(pcm.iter().flat_map(|s| s.to_le_bytes()));
                    if batch.len() >= 1280 { // 40 ms
                        tx.send(Message::binary(std::mem::take(&mut batch))).await?;
                    }
                }
            }
            msg = rx.next() => {
                let Some(msg) = msg else { break };
                match msg? {
                    Message::Binary(b) => {
                        voice.push(app, &b);
                        last_activity = Instant::now();
                    }
                    Message::Text(t) => {
                        let Ok(m) = serde_json::from_str::<ServerMsg>(&t) else { continue };
                        match m.kind.as_str() {
                            "ready" => {
                                ready = true;
                                for c in std::mem::take(&mut held).chunks(1280) {
                                    tx.send(Message::binary(c.to_vec())).await?;
                                }
                            }
                            "text" => { app.said(&m.who, &m.text); last_activity = Instant::now(); }
                            "tool" => if m.running { app.log(format!("Werkzeug: {}", m.name)); last_activity = Instant::now(); },
                            "interrupted" => app.clear_playback(),
                            "end" => {
                                outcome = if m.reason.is_empty() { "aufgelegt".into() } else { m.reason };
                                ending.get_or_insert_with(Instant::now);
                            }
                            "error" => {
                                outcome = format!("Fehler: {}", m.message);
                                app.log(format!("IDA: {}", m.message));
                                ending.get_or_insert_with(Instant::now);
                            }
                            _ => {}
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            Some(cmd) = commands.recv() => {
                if matches!(cmd, Command::Stop) {
                    outcome = "auf der Website beendet".into();
                    let _ = tx.send(Message::text(r#"{"type":"end"}"#)).await;
                    app.clear_playback();
                    break;
                }
            }
            _ = tick.tick() => {
                if app.playing() {
                    last_activity = Instant::now();
                }
                if let Some(at) = ending {
                    // Erst ausspielen, was noch in der Warteschlange liegt, dann schließen
                    if !app.playing() || at.elapsed() > Duration::from_secs(20) {
                        break;
                    }
                } else if last_activity.elapsed() > silence {
                    outcome = "Stille".into();
                    let _ = tx.send(Message::text(r#"{"type":"end"}"#)).await;
                    ending = Some(Instant::now());
                } else if started.elapsed() > max {
                    outcome = "Zeitlimit".into();
                    let _ = tx.send(Message::text(r#"{"type":"end"}"#)).await;
                    ending = Some(Instant::now());
                }
            }
        }
    }
    app.talking.store(false, Ordering::Relaxed);
    *app.mic.lock().unwrap() = None;
    let _ = tx.close().await;
    // Rest abspielen lassen (höchstens 15 s), dann Schlusston
    let until = now_ms() + 15_000;
    while app.playing() && now_ms() < until {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if cfg.audio.tones {
        audio::beep(app, Beep::End);
    }
    app.log(format!("Gespräch zu Ende ({outcome})"));
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_urls() {
        assert_eq!(
            ws_url("http://10.0.0.5:8086", "a.b.c"),
            "ws://10.0.0.5:8086/live?ticket=a.b.c&mode=speaker"
        );
        assert_eq!(
            ws_url("https://voice.example.com/", "t"),
            "wss://voice.example.com/live?ticket=t&mode=speaker"
        );
    }

    #[test]
    fn quick_ticket_body() {
        let mut cfg = Config::default();
        cfg.assistant.prompt = "Wohnzimmer".into();
        cfg.wake.second_word = "hey computer".into();
        cfg.quick.prompt = "Knapp.".into();
        let normal = ticket_body(&cfg, false);
        assert_eq!(normal["prompt"], "Wohnzimmer");
        assert!(normal.get("oneshot").is_none());
        let q = ticket_body(&cfg, true);
        assert_eq!(q["prompt"], "Wohnzimmer\n\nKnapp.");
        assert_eq!(q["name"], "Hey Computer");
        assert_eq!(q["voice"], "Puck");
        assert_eq!(q["oneshot"], true);
        assert_eq!(q["smart_home"], true);
        cfg.quick.name = "Jarvis".into();
        assert_eq!(ticket_body(&cfg, true)["name"], "Jarvis");
    }

    #[test]
    fn parses_server_messages() {
        let m: ServerMsg = serde_json::from_str(r#"{"type":"end","reason":"done"}"#).unwrap();
        assert_eq!(m.kind, "end");
        assert_eq!(m.reason, "done");
        let m: ServerMsg = serde_json::from_str(
            r#"{"type":"ready","rate":24000,"agent":"Stella","offline":false}"#,
        )
        .unwrap();
        assert_eq!(m.kind, "ready");
    }
}
