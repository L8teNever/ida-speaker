//! Einstellungen: liegen nur auf dem Gerät in `/data/config.toml` (nie im Repo) und werden über die Website geändert.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub web: Web,
    pub ida: Ida,
    pub audio: Audio,
    pub wake: Wake,
    pub echo: Echo,
    pub call: Call,
    pub cloudflare: Cloudflare,
    pub wifi: Wifi,
    pub assistant: Assistant,
    pub presence: Presence,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Web {
    /// Argon2-Hash des Passworts der Einstellungsseite; leer = Einrichtung noch nicht gemacht.
    pub password_hash: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Ida {
    /// Adresse des Voice-Dienstes, z. B. `http://10.0.0.5:8086` (im Cloudflare-Netz) oder `https://voice.example.com`.
    pub url: String,
    /// Name des Lautsprechers, wie in `VOICE_DEVICES` bei IDA.
    pub device_name: String,
    /// Schlüssel des Lautsprechers (aus IDA, `GET /api/v1/voice/devices`).
    pub device_key: String,
    /// Wie der Lautsprecher IDA erreicht: "domain" (öffentliche Adresse, ggf. hinter Cloudflare Access),
    /// "warp" (privates Netz über Cloudflare WARP) oder "lan" (gleiches Netz).
    pub via: String,
    /// Service-Token für Cloudflare Access vor der Voice-Domain (leer = keine Access-Sperre).
    pub access_client_id: String,
    pub access_client_secret: String,
    /// Einrichtungs-Assistent einmal durchlaufen.
    pub setup_done: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Audio {
    /// Geräte-ID von cpal (leer = Standardgerät).
    pub input: String,
    pub output: String,
    /// Lautstärke in Prozent (0–200).
    pub volume: u32,
    /// Mikrofon-Verstärkung in Prozent (0–400).
    pub mic_gain: u32,
    /// Signaltöne beim Zuhören und am Gesprächsende.
    pub tones: bool,
    /// Gewählter Bluetooth-Lautsprecher (MAC) und sein Name; Ausgabe läuft dann über `alsa:ida_bluetooth`.
    pub bluetooth: String,
    pub bluetooth_name: String,
}

impl Default for Audio {
    fn default() -> Self {
        Self {
            input: String::new(),
            output: String::new(),
            volume: 100,
            mic_gain: 100,
            tones: true,
            bluetooth: String::new(),
            bluetooth_name: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Wake {
    pub enabled: bool,
    /// Ein Wort oder eine kurze Phrase, z. B. "stella" oder "hey stella".
    pub word: String,
    /// 0.0 = streng (wenig Fehlalarme), 1.0 = empfindlich.
    pub sensitivity: f32,
}

impl Default for Wake {
    fn default() -> Self {
        Self {
            enabled: true,
            word: "stella".into(),
            sensitivity: 0.5,
        }
    }
}

impl Wake {
    /// Mindest-Sicherheit von Vosk pro Wort, abgeleitet aus der Empfindlichkeit.
    pub fn min_conf(&self) -> f32 {
        (0.95 - self.sensitivity.clamp(0.0, 1.0) * 0.6).clamp(0.3, 0.95)
    }

    /// Normalisierte Wörter der Phrase (klein, nur Buchstaben).
    pub fn words(&self) -> Vec<String> {
        normalize_phrase(&self.word)
    }
}

pub fn normalize_phrase(s: &str) -> Vec<String> {
    s.split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphabetic() || *c == '\'')
                .flat_map(char::to_lowercase)
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .take(4)
        .collect()
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Off,
    Low,
    Moderate,
    High,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Echo {
    /// WebRTC-Echo-Unterdrückung (AEC3): Stellas Stimme wird aus dem Mikrofon herausgerechnet.
    pub enabled: bool,
    pub noise: Level,
    /// Automatische Mikrofon-Lautstärke.
    pub agc: bool,
    /// Notfall-Modus für schwierige Räume: Mikrofon stumm, solange Stella spricht (dann kein Unterbrechen).
    pub mute_while_speaking: bool,
}

impl Default for Echo {
    fn default() -> Self {
        Self {
            enabled: true,
            noise: Level::Moderate,
            agc: true,
            mute_while_speaking: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Call {
    /// Gespräch endet nach so vielen Sekunden ohne Sprache auf beiden Seiten.
    pub silence_end_s: u32,
    /// Spätestens nach so vielen Minuten wird aufgelegt.
    pub max_minutes: u32,
    /// Pegel (0–32767, RMS nach dem Echo-Filter), ab dem das Mikrofon als „jemand spricht“ gilt.
    pub speech_rms: u32,
}

impl Default for Call {
    fn default() -> Self {
        Self {
            silence_end_s: 10,
            max_minutes: 15,
            speech_rms: 600,
        }
    }
}

/// Nur für Gespräche über diesen Lautsprecher: eigene Anweisungen an Stella und Smart Home ohne Nachfrage.
/// Die normalen Regeln von IDA gelten weiter, das hier kommt nur dazu.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Assistant {
    /// z. B. „Ich stehe im Wohnzimmer. Licht heißt light.wohnzimmer.“ (höchstens 4000 Zeichen)
    pub prompt: String,
    /// Licht & Co. schaltet Stella sofort, ohne nachzufragen (Home Assistant).
    pub smart_home: bool,
}

/// Nur reagieren, wenn mein Handy da ist (Bluetooth-Anwesenheit).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Presence {
    pub enabled: bool,
    /// Gekoppeltes Handy (MAC) und sein Name.
    pub mac: String,
    pub name: String,
    /// So lange ohne Antwort, dann gilt das Handy als weg.
    pub away_after_s: u32,
    /// Nur wenn die Signalstärke bekannt ist: schwächer als das (dBm) gilt als zu weit weg. 0 = egal.
    pub min_rssi: i32,
}

impl Default for Presence {
    fn default() -> Self {
        Self {
            enabled: false,
            mac: String::new(),
            name: String::new(),
            away_after_s: 60,
            min_rssi: 0,
        }
    }
}

/// WLAN über NetworkManager des Pi. Ohne Netz macht der Lautsprecher ein eigenes Einrichtungs-WLAN auf.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Wifi {
    pub hotspot: bool,
    pub hotspot_ssid: String,
    /// Mindestens 8 Zeichen (WPA2).
    pub hotspot_password: String,
}

impl Default for Wifi {
    fn default() -> Self {
        Self {
            hotspot: true,
            hotspot_ssid: "IDA-Speaker-Setup".into(),
            hotspot_password: "idaspeaker".into(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Cloudflare {
    /// WARP im Container starten und mit Zero Trust verbinden.
    pub enabled: bool,
    /// Team-Name von Zero Trust (`<team>.cloudflareaccess.com`).
    pub team: String,
    pub client_id: String,
    pub client_secret: String,
}

impl Cloudflare {
    pub fn ready(&self) -> bool {
        self.enabled
            && !self.team.trim().is_empty()
            && !self.client_id.trim().is_empty()
            && !self.client_secret.trim().is_empty()
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Schreibt atomar (erst Datei daneben, dann umbenennen) und nur für den Besitzer lesbar.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, toml::to_string_pretty(self)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Für die Website: Geheimnisse nie zurückgeben, nur ob sie gesetzt sind.
    pub fn public(&self) -> serde_json::Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        v["web"] = serde_json::json!({ "password_set": !self.web.password_hash.is_empty() });
        v["ida"]["device_key"] = serde_json::Value::String(mask(&self.ida.device_key));
        v["ida"]["access_client_secret"] =
            serde_json::Value::String(mask(&self.ida.access_client_secret));
        v["wifi"]["hotspot_password"] =
            serde_json::Value::String(mask(&self.wifi.hotspot_password));
        v["cloudflare"]["client_secret"] =
            serde_json::Value::String(mask(&self.cloudflare.client_secret));
        v
    }

    /// Übernimmt Änderungen von der Website. Leere Geheimnisse bedeuten „unverändert“; das Passwort ändert sich hier nie.
    pub fn merge(&self, patch: serde_json::Value) -> anyhow::Result<Self> {
        let mut base = serde_json::to_value(self)?;
        merge_json(&mut base, patch);
        let mut next: Config = serde_json::from_value(base)?;
        next.web = self.web.clone();
        if next.ida.device_key.trim().is_empty()
            || next.ida.device_key == mask(&self.ida.device_key)
        {
            next.ida.device_key = self.ida.device_key.clone();
        }
        if next.cloudflare.client_secret.trim().is_empty()
            || next.cloudflare.client_secret == mask(&self.cloudflare.client_secret)
        {
            next.cloudflare.client_secret = self.cloudflare.client_secret.clone();
        }
        if next.wifi.hotspot_password == mask(&self.wifi.hotspot_password)
            || next.wifi.hotspot_password.chars().count() < 8
        {
            next.wifi.hotspot_password = self.wifi.hotspot_password.clone();
        }
        next.wifi.hotspot_ssid = next.wifi.hotspot_ssid.trim().chars().take(32).collect();
        if next.wifi.hotspot_ssid.is_empty() {
            next.wifi.hotspot_ssid = Wifi::default().hotspot_ssid;
        }
        next.assistant.prompt = next.assistant.prompt.trim().chars().take(4000).collect();
        if next.ida.access_client_secret == mask(&self.ida.access_client_secret)
            || (next.ida.access_client_secret.trim().is_empty()
                && !next.ida.access_client_id.trim().is_empty())
        {
            next.ida.access_client_secret = self.ida.access_client_secret.clone();
        }
        if next.ida.access_client_id.trim().is_empty() {
            next.ida.access_client_secret.clear();
        }
        next.ida.access_client_id = next.ida.access_client_id.trim().to_string();
        next.ida.access_client_secret = next.ida.access_client_secret.trim().to_string();
        if !matches!(next.ida.via.as_str(), "domain" | "warp" | "lan") {
            next.ida.via = String::new();
        }
        next.presence.away_after_s = next.presence.away_after_s.clamp(15, 1800);
        next.presence.min_rssi = next.presence.min_rssi.clamp(-100, 0);
        if next.presence.mac.is_empty() {
            next.presence.enabled = false;
        }
        next.ida.url = next.ida.url.trim().trim_end_matches('/').to_string();
        next.ida.device_name = next.ida.device_name.trim().to_lowercase();
        next.ida.device_key = next.ida.device_key.trim().to_string();
        next.cloudflare.team = next.cloudflare.team.trim().to_string();
        next.cloudflare.client_id = next.cloudflare.client_id.trim().to_string();
        next.cloudflare.client_secret = next.cloudflare.client_secret.trim().to_string();
        next.audio.volume = next.audio.volume.min(200);
        next.audio.mic_gain = next.audio.mic_gain.min(400);
        next.wake.sensitivity = next.wake.sensitivity.clamp(0.0, 1.0);
        next.wake.word = normalize_phrase(&next.wake.word).join(" ");
        if next.wake.word.is_empty() {
            next.wake.word = self.wake.word.clone();
        }
        next.call.silence_end_s = next.call.silence_end_s.clamp(3, 120);
        next.call.max_minutes = next.call.max_minutes.clamp(1, 120);
        Ok(next)
    }
}

pub fn mask(secret: &str) -> String {
    if secret.is_empty() {
        String::new()
    } else {
        "••••••••".into()
    }
}

fn merge_json(base: &mut serde_json::Value, patch: serde_json::Value) {
    match (base, patch) {
        (serde_json::Value::Object(b), serde_json::Value::Object(p)) => {
            for (k, v) in p {
                merge_json(b.entry(k).or_insert(serde_json::Value::Null), v);
            }
        }
        (b, p) => *b = p,
    }
}

pub fn data_dir() -> PathBuf {
    std::env::var_os("IDA_SPEAKER_DATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/data"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_defaults() {
        let dir = std::env::temp_dir().join(format!("ida-speaker-test-{}", std::process::id()));
        let path = dir.join("config.toml");
        assert_eq!(Config::load(&path).unwrap(), Config::default());
        let mut c = Config::default();
        c.ida.url = "http://10.0.0.5:8086".into();
        c.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), c);
        // Unbekannte/fehlende Felder brechen nichts
        std::fs::write(&path, "[wake]\nword = \"hey stella\"\n").unwrap();
        let c = Config::load(&path).unwrap();
        assert_eq!(c.wake.word, "hey stella");
        assert_eq!(c.audio.volume, 100);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn merge_keeps_secrets_and_password() {
        let mut c = Config::default();
        c.web.password_hash = "hash".into();
        c.ida.device_key = "geheim".into();
        let next = c
            .merge(serde_json::json!({
                "web": {"password_hash": "evil"},
                "ida": {"device_key": "", "url": " http://x:8086/ ", "device_name": "Wohnzimmer"},
                "wake": {"word": "Hey, Stella!", "sensitivity": 7.0},
                "audio": {"volume": 999}
            }))
            .unwrap();
        assert_eq!(next.web.password_hash, "hash");
        assert_eq!(next.ida.device_key, "geheim");
        assert_eq!(next.ida.url, "http://x:8086");
        assert_eq!(next.ida.device_name, "wohnzimmer");
        assert_eq!(next.wake.word, "hey stella");
        assert_eq!(next.wake.sensitivity, 1.0);
        assert_eq!(next.audio.volume, 200);
        let masked = c.public();
        assert_eq!(masked["ida"]["device_key"], "••••••••");
        assert!(masked["web"].get("password_hash").is_none());
        let same = c
            .merge(serde_json::json!({"ida": {"device_key": "••••••••"}}))
            .unwrap();
        assert_eq!(same.ida.device_key, "geheim");
    }

    #[test]
    fn sensitivity_maps_to_confidence() {
        let mut w = Wake {
            sensitivity: 0.0,
            ..Default::default()
        };
        assert!((w.min_conf() - 0.95).abs() < 1e-6);
        w.sensitivity = 1.0;
        assert!((w.min_conf() - 0.35).abs() < 1e-6);
    }
}
