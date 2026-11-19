//! Einstellungs-Website auf Port 8080: Einrichtung, Status, Geräte, Wake-Word, Cloudflare und Tests.
//! Geschützt mit einem Passwort, das beim ersten Öffnen festgelegt wird.

use crate::audio::{self, Beep};
use crate::state::{App, Command, Phase, now_s};
use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use axum::Router;
use axum::extract::{Json, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use rand::RngCore;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::Ordering;

const INDEX: &str = include_str!("../web/index.html");
const COOKIE: &str = "ida_speaker";
const SESSION_S: u64 = 30 * 24 * 3600;

type S = State<Arc<App>>;

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(|| async { Html(INDEX) }))
        .route("/health", get(|| async { "ok" }))
        .route("/api/status", get(status))
        .route("/api/setup", post(setup))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/password", post(password))
        .route("/api/state", get(state))
        .route("/api/level", get(level))
        .route("/api/devices", get(devices))
        .route("/api/config", post(config))
        .route("/api/test/ida", post(test_ida))
        .route("/api/test/tone", post(test_tone))
        .route("/api/talk", post(talk))
        .route("/api/stop", post(stop))
        .route("/api/mute", post(mute))
        .route("/api/warp/reconnect", post(warp_reconnect))
        .route("/api/restart", post(restart))
        .route("/api/power", post(power))
        .route("/api/wifi", get(wifi_list))
        .route("/api/wifi/connect", post(wifi_connect))
        .route("/api/wifi/forget", post(wifi_forget))
        .route("/api/cloudflare/auto", post(cloudflare_auto))
        .with_state(app)
}

fn err(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(json!({ "detail": msg.into() }))).into_response()
}

fn token_from(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|p| {
        p.trim()
            .strip_prefix(&format!("{COOKIE}="))
            .map(str::to_string)
    })
}

fn authed(app: &App, headers: &HeaderMap) -> bool {
    let Some(t) = token_from(headers) else {
        return false;
    };
    let mut sessions = app.sessions.lock().unwrap();
    let now = now_s();
    sessions.retain(|_, exp| *exp > now);
    sessions.contains_key(&t)
}

/// Prüft die Anmeldung; gibt die Fehlerantwort zurück, wenn sie fehlt.
#[allow(clippy::result_large_err)]
fn guard(app: &App, headers: &HeaderMap) -> Result<(), Response> {
    if app.cfg().web.password_hash.is_empty() {
        return Err(err(StatusCode::FORBIDDEN, "Erst ein Passwort festlegen"));
    }
    if authed(app, headers) {
        app.last_web.store(now_s(), Ordering::Relaxed);
        Ok(())
    } else {
        Err(err(StatusCode::UNAUTHORIZED, "Bitte anmelden"))
    }
}

fn new_session(app: &App) -> Response {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    app.sessions
        .lock()
        .unwrap()
        .insert(token.clone(), now_s() + SESSION_S);
    let cookie =
        format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={SESSION_S}");
    ([(header::SET_COOKIE, cookie)], Json(json!({ "ok": true }))).into_response()
}

fn hash(pw: &str) -> anyhow::Result<String> {
    let mut salt = [0u8; 16];
    rand::rng().fill_bytes(&mut salt);
    let salt = SaltString::encode_b64(&salt).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(Argon2::default()
        .hash_password(pw.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .to_string())
}

fn verify(pw: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .map(|h| Argon2::default().verify_password(pw.as_bytes(), &h).is_ok())
        .unwrap_or(false)
}

#[derive(Deserialize)]
struct Pw {
    password: String,
    #[serde(default)]
    new_password: String,
}

async fn status(State(app): S, headers: HeaderMap) -> Json<Value> {
    Json(json!({
        "setup_needed": app.cfg().web.password_hash.is_empty(),
        "logged_in": authed(&app, &headers),
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn setup(State(app): S, Json(p): Json<Pw>) -> Response {
    let cfg = app.cfg();
    if !cfg.web.password_hash.is_empty() {
        return err(StatusCode::FORBIDDEN, "Passwort ist schon festgelegt");
    }
    if p.password.chars().count() < 8 {
        return err(StatusCode::BAD_REQUEST, "Mindestens 8 Zeichen");
    }
    let mut next = cfg;
    match hash(&p.password) {
        Ok(h) => next.web.password_hash = h,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
    if let Err(e) = app.save_config(next) {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Speichern fehlgeschlagen: {e}"),
        );
    }
    app.log("Passwort für die Einstellungsseite festgelegt");
    new_session(&app)
}

async fn login(State(app): S, Json(p): Json<Pw>) -> Response {
    let h = app.cfg().web.password_hash;
    let ok = tokio::task::spawn_blocking(move || verify(&p.password, &h))
        .await
        .unwrap_or(false);
    if !ok {
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        return err(StatusCode::UNAUTHORIZED, "Passwort falsch");
    }
    new_session(&app)
}

async fn logout(State(app): S, headers: HeaderMap) -> Response {
    if let Some(t) = token_from(&headers) {
        app.sessions.lock().unwrap().remove(&t);
    }
    (
        [(header::SET_COOKIE, format!("{COOKIE}=; Path=/; Max-Age=0"))],
        Json(json!({ "ok": true })),
    )
        .into_response()
}

async fn password(State(app): S, headers: HeaderMap, Json(p): Json<Pw>) -> Response {
    if let Err(r) = guard(&app, &headers) {
        return r;
    }
    let cfg = app.cfg();
    if !verify(&p.password, &cfg.web.password_hash) {
        return err(StatusCode::UNAUTHORIZED, "Altes Passwort falsch");
    }
    if p.new_password.chars().count() < 8 {
        return err(StatusCode::BAD_REQUEST, "Mindestens 8 Zeichen");
    }
    let mut next = cfg;
    next.web.password_hash = match hash(&p.new_password) {
        Ok(h) => h,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    if let Err(e) = app.save_config(next) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    app.sessions.lock().unwrap().clear();
    new_session(&app)
}

async fn state(State(app): S, headers: HeaderMap) -> Response {
    if let Err(r) = guard(&app, &headers) {
        return r;
    }
    let cfg = app.cfg();
    let s = app.status.lock().unwrap();
    Json(json!({
        "phase": s.phase,
        "muted": app.muted(),
        "config": cfg.public(),
        "audio": s.audio,
        "warp": s.warp,
        "wifi": s.wifi,
        "triggers": s.triggers,
        "logs": s.logs.iter().rev().take(80).collect::<Vec<_>>(),
        "conversation": s.last_text.iter().map(|(w, t)| json!({"who": w, "text": t})).collect::<Vec<_>>(),
        "min_conf": cfg.wake.min_conf(),
    }))
    .into_response()
}

async fn level(State(app): S, headers: HeaderMap) -> Response {
    if let Err(r) = guard(&app, &headers) {
        return r;
    }
    let s = app.status.lock().unwrap();
    Json(json!({ "level": app.level.load(Ordering::Relaxed), "phase": s.phase, "heard": s.audio.heard })).into_response()
}

async fn devices(State(app): S, headers: HeaderMap) -> Response {
    if let Err(r) = guard(&app, &headers) {
        return r;
    }
    Json(
        tokio::task::spawn_blocking(audio::list_devices)
            .await
            .unwrap_or_default(),
    )
    .into_response()
}

async fn config(State(app): S, headers: HeaderMap, Json(patch): Json<Value>) -> Response {
    if let Err(r) = guard(&app, &headers) {
        return r;
    }
    let next = match app.cfg().merge(patch) {
        Ok(n) => n,
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                format!("Ungültige Einstellungen: {e}"),
            );
        }
    };
    if let Err(e) = app.save_config(next.clone()) {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Speichern fehlgeschlagen: {e}"),
        );
    }
    app.log("Einstellungen gespeichert");
    Json(json!({ "ok": true, "config": next.public() })).into_response()
}

async fn test_ida(State(app): S, headers: HeaderMap) -> Response {
    if let Err(r) = guard(&app, &headers) {
        return r;
    }
    match crate::session::ticket(&app.cfg()).await {
        Ok(t) => Json(json!({ "ok": true, "message": format!("Verbunden – {} ist bereit{}", if t.name.is_empty() { "IDA" } else { &t.name }, if t.busy { " (gerade beschäftigt)" } else { "" }) })).into_response(),
        Err(e) => Json(json!({ "ok": false, "message": format!("{e:#}") })).into_response(),
    }
}

async fn test_tone(State(app): S, headers: HeaderMap) -> Response {
    if let Err(r) = guard(&app, &headers) {
        return r;
    }
    audio::beep(&app, Beep::Test);
    Json(json!({ "ok": true })).into_response()
}

async fn talk(State(app): S, headers: HeaderMap) -> Response {
    if let Err(r) = guard(&app, &headers) {
        return r;
    }
    if matches!(app.phase(), Phase::Talking | Phase::Connecting) {
        return err(StatusCode::CONFLICT, "Gespräch läuft schon");
    }
    let _ = app.commands.send(Command::Start("Website".into()));
    Json(json!({ "ok": true })).into_response()
}

async fn stop(State(app): S, headers: HeaderMap) -> Response {
    if let Err(r) = guard(&app, &headers) {
        return r;
    }
    let _ = app.commands.send(Command::Stop);
    Json(json!({ "ok": true })).into_response()
}

#[derive(Deserialize)]
struct Mute {
    on: bool,
}

async fn mute(State(app): S, headers: HeaderMap, Json(m): Json<Mute>) -> Response {
    if let Err(r) = guard(&app, &headers) {
        return r;
    }
    app.muted.store(m.on, Ordering::Relaxed);
    if !matches!(
        app.phase(),
        Phase::Talking | Phase::Connecting | Phase::Error | Phase::Starting
    ) {
        app.set_phase(if m.on { Phase::Muted } else { Phase::Listening });
    }
    app.log(if m.on {
        "Stumm: hört nicht auf das Wake-Word"
    } else {
        "Hört wieder zu"
    });
    Json(json!({ "ok": true })).into_response()
}

async fn warp_reconnect(State(app): S, headers: HeaderMap) -> Response {
    if let Err(r) = guard(&app, &headers) {
        return r;
    }
    let out = crate::warp::reconnect().await;
    app.log(format!("WARP neu verbinden: {out}"));
    Json(json!({ "ok": true, "message": out })).into_response()
}

async fn restart(State(app): S, headers: HeaderMap) -> Response {
    if let Err(r) = guard(&app, &headers) {
        return r;
    }
    app.log("Neustart auf Wunsch");
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        std::process::exit(0); // Docker startet den Container neu (restart: unless-stopped)
    });
    Json(json!({ "ok": true })).into_response()
}

#[derive(Deserialize)]
struct Power {
    action: String,
}

async fn power(State(app): S, headers: HeaderMap, Json(p): Json<Power>) -> Response {
    if let Err(r) = guard(&app, &headers) {
        return r;
    }
    app.log(format!(
        "Pi: {}",
        if p.action == "reboot" {
            "Neustart"
        } else {
            "Ausschalten"
        }
    ));
    match crate::system::power(&p.action).await {
        Ok(()) => Json(json!({ "ok": true, "message": "Wird ausgeführt …" })).into_response(),
        Err(e) => Json(json!({ "ok": false, "message": format!("{e:#}") })).into_response(),
    }
}

async fn wifi_list(State(app): S, headers: HeaderMap) -> Response {
    if let Err(r) = guard(&app, &headers) {
        return r;
    }
    let status = crate::wifi::status().await;
    app.status.lock().unwrap().wifi = status.clone();
    let (networks, error) = if status.available {
        match crate::wifi::scan().await {
            Ok(n) => (n, String::new()),
            Err(e) => (vec![], format!("{e:#}")),
        }
    } else {
        (vec![], String::new())
    };
    Json(json!({ "status": status, "networks": networks, "error": error })).into_response()
}

#[derive(Deserialize)]
struct WifiConnect {
    ssid: String,
    #[serde(default)]
    password: String,
}

async fn wifi_connect(State(app): S, headers: HeaderMap, Json(w): Json<WifiConnect>) -> Response {
    if let Err(r) = guard(&app, &headers) {
        return r;
    }
    if w.ssid.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "WLAN-Name fehlt");
    }
    let on_hotspot = app.status.lock().unwrap().wifi.hotspot;
    app.log(format!("Verbinde mit WLAN „{}“", w.ssid));
    if on_hotspot {
        // Die Seite läuft gerade über das Einrichtungs-WLAN: erst antworten, dann umschalten
        let app2 = app.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            match crate::wifi::connect(&w.ssid, &w.password).await {
                Ok(_) => app2.log(format!("Mit „{}“ verbunden", w.ssid)),
                Err(e) => app2.log(format!("WLAN „{}“: {e:#}", w.ssid)),
            }
        });
        let host = app.status.lock().unwrap().wifi.hostname.clone();
        return Json(json!({ "ok": true, "message": format!(
            "Verbinde … Das Einrichtungs-WLAN geht jetzt aus. Verbinde dein Handy wieder mit deinem WLAN und öffne http://{}.local:8080. Klappt es nicht, ist das Einrichtungs-WLAN nach ein paar Minuten wieder da.",
            if host.is_empty() { "raspberrypi" } else { &host }) })).into_response();
    }
    match crate::wifi::connect(&w.ssid, &w.password).await {
        Ok(_) => Json(json!({ "ok": true, "message": format!("Mit „{}“ verbunden", w.ssid) }))
            .into_response(),
        Err(e) => Json(json!({ "ok": false, "message": format!("{e:#}") })).into_response(),
    }
}

#[derive(Deserialize)]
struct WifiForget {
    ssid: String,
}

async fn wifi_forget(State(app): S, headers: HeaderMap, Json(w): Json<WifiForget>) -> Response {
    if let Err(r) = guard(&app, &headers) {
        return r;
    }
    match crate::wifi::forget(&w.ssid).await {
        Ok(()) => Json(json!({ "ok": true, "message": format!("„{}“ vergessen", w.ssid) }))
            .into_response(),
        Err(e) => Json(json!({ "ok": false, "message": format!("{e:#}") })).into_response(),
    }
}

async fn cloudflare_auto(
    State(app): S,
    headers: HeaderMap,
    Json(req): Json<crate::cloudflare::Request>,
) -> Response {
    if let Err(r) = guard(&app, &headers) {
        return r;
    }
    Json(crate::cloudflare::auto(&app, req).await).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_roundtrip() {
        let h = hash("richtig-lang").unwrap();
        assert!(verify("richtig-lang", &h));
        assert!(!verify("falsch", &h));
        assert!(!verify("x", "kein-hash"));
    }

    #[test]
    fn cookie_parsing() {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, "a=1; ida_speaker=abc; b=2".parse().unwrap());
        assert_eq!(token_from(&h).as_deref(), Some("abc"));
    }
}
