//! Wake-Word lokal und offline mit Vosk: eine kleine Grammatik aus dem eingestellten Wort und "[unk]" für alles andere.
//! So lässt sich das Wort auf der Website frei ändern, ohne ein Modell zu trainieren.

use crate::config::Wake;
use std::path::{Path, PathBuf};
use vosk::{CompleteResult, DecodingState, Model, Recognizer};

pub struct Detector {
    rec: Option<Recognizer>,
    /// (0 = Hauptname / 1 = zweiter Name, Phrase) – leere Phrase = aus
    phrases: Vec<(usize, Vec<String>)>,
    min_conf: f32,
    /// Wörter der Phrase, die das Modell nicht kennt (die Website zeigt sie an).
    pub unknown: Vec<String>,
    /// Zuletzt gehörter Text (für „Jetzt sagen“ auf der Website).
    pub heard: String,
    /// Was der Erkennung zuletzt gegeben wurde (für den Befehl direkt nach dem Namen) und wie viel seit dem Zurücksetzen.
    fed: std::collections::VecDeque<i16>,
    fed_total: u64,
}

/// Erkanntes Wake-Word: welches (0 = Hauptname, 1 = zweiter Name) und was danach schon gesagt wurde.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub which: usize,
    /// Audio direkt nach dem Namen („Computer, Licht an“ in einem Atemzug), 16 kHz.
    pub after: Vec<i16>,
}

/// So viel Audio hebt der Detektor auf (8 s).
const KEEP: usize = 8 * crate::dsp::RATE as usize;

/// Sucht das Sprachmodell: zuerst ein eigenes in `/data/model`, sonst das im Image.
pub fn model_path(data: &Path) -> Option<PathBuf> {
    let own = data.join("model");
    let builtin = std::env::var_os("VOSK_MODEL")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/opt/vosk-model"));
    [own, builtin]
        .into_iter()
        .find(|p| p.join("am").is_dir() || p.join("conf").is_dir())
}

pub fn load_model(data: &Path) -> Option<Model> {
    vosk::set_log_level(vosk::LogLevel::Error);
    let path = model_path(data)?;
    let model = Model::new(path.to_string_lossy().to_string());
    if model.is_none() {
        tracing::error!("Vosk-Modell in {} lässt sich nicht laden", path.display());
    }
    model
}

impl Detector {
    pub fn new(model: Option<&mut Model>, cfg: &Wake) -> Self {
        let phrases = cfg.phrases();
        let all: Vec<String> = phrases.iter().flat_map(|(_, p)| p).cloned().collect();
        // Nur die eingestellten Namen melden, Aussprachen kommen ja aus dem Modell
        let named: Vec<String> = phrases[..2].iter().flat_map(|(_, p)| p).cloned().collect();
        let mut unknown = Vec::new();
        let rec = match model {
            Some(model) if !phrases[0].1.is_empty() => {
                for w in &named {
                    if model.find_word(w).is_none() && !unknown.contains(w) {
                        unknown.push(w.clone());
                    }
                }
                // Lockwörter: ähnlich klingende und häufige Wörter fangen ab, was sonst als Wake-Word durchginge
                let mut grammar: Vec<String> = phrases
                    .iter()
                    .filter(|(_, p)| !p.is_empty())
                    .map(|(_, p)| p.join(" "))
                    .collect();
                grammar.dedup();
                grammar.extend(
                    decoys(&all)
                        .into_iter()
                        .filter(|d| model.find_word(d).is_some()),
                );
                grammar.push("[unk]".into());
                let rec = Recognizer::new_with_grammar(model, crate::dsp::RATE as f32, &grammar);
                rec.map(|mut r| {
                    r.set_words(true);
                    r
                })
            }
            _ => None,
        };
        Self {
            rec,
            phrases,
            min_conf: cfg.min_conf(),
            unknown,
            heard: String::new(),
            fed: std::collections::VecDeque::with_capacity(KEEP),
            fed_total: 0,
        }
    }

    pub fn ready(&self) -> bool {
        self.rec.is_some()
    }

    /// Nimmt 16-kHz-Audio und meldet, sobald ein Wake-Word sicher erkannt ist.
    pub fn feed(&mut self, pcm: &[i16]) -> Option<Hit> {
        let rec = self.rec.as_mut()?;
        self.fed.extend(pcm.iter().copied());
        while self.fed.len() > KEEP {
            self.fed.pop_front();
        }
        self.fed_total += pcm.len() as u64;
        let Ok(DecodingState::Finalized) = rec.accept_waveform(pcm) else {
            return None;
        };
        let found: Vec<(String, f32, f32)> = match rec.result() {
            CompleteResult::Single(r) => r
                .result
                .iter()
                .map(|w| (w.word.to_string(), w.conf, w.end))
                .collect(),
            CompleteResult::Multiple(m) => m
                .alternatives
                .first()
                .map(|a| {
                    a.result
                        .iter()
                        .map(|w| (w.word.to_string(), a.confidence, w.end))
                        .collect()
                })
                .unwrap_or_default(),
        };
        let text: Vec<&str> = found
            .iter()
            .map(|(w, _, _)| w.as_str())
            .filter(|w| *w != "[unk]")
            .collect();
        if !text.is_empty() {
            self.heard = text.join(" ");
        }
        let words: Vec<(String, f32)> = found.iter().map(|(w, c, _)| (w.clone(), *c)).collect();
        for (which, phrase) in &self.phrases {
            if let Some(last) = find(&words, phrase, self.min_conf) {
                let end = found[last].2;
                return Some(Hit {
                    which: *which,
                    after: self.after(end),
                });
            }
        }
        None
    }

    /// Audio ab Sekunde `end` (Zeit der Erkennung seit dem Zurücksetzen) bis jetzt.
    fn after(&self, end: f32) -> Vec<i16> {
        let start = (end.max(0.0) * crate::dsp::RATE as f32) as u64;
        let ring_start = self.fed_total - self.fed.len() as u64;
        let from = start.saturating_sub(ring_start) as usize;
        self.fed.iter().skip(from).copied().collect()
    }

    pub fn reset(&mut self) {
        if let Some(r) = self.rec.as_mut() {
            r.reset();
        }
        self.fed.clear();
        self.fed_total = 0;
    }
}

/// Freie Erkennung ohne Grammatik: was das Modell in einer Aufnahme hört (für „Namen einsprechen“).
pub fn transcribe(model: &Model, pcm: &[i16]) -> String {
    let Some(mut rec) = Recognizer::new(model, crate::dsp::RATE as f32) else {
        return String::new();
    };
    let mut text = Vec::new();
    for chunk in pcm.chunks(1600) {
        if let Ok(DecodingState::Finalized) = rec.accept_waveform(chunk)
            && let CompleteResult::Single(r) = rec.result()
        {
            text.push(r.text.to_string());
        }
    }
    if let CompleteResult::Single(r) = rec.final_result() {
        text.push(r.text.to_string());
    }
    text.join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Taugt eine gehörte Phrase als weitere Aussprache? Nicht leer, nicht zu kurz, kein Allerweltswort.
pub fn usable_alias(p: &[String]) -> bool {
    !p.is_empty()
        && p.iter().map(|w| w.chars().count()).sum::<usize>() >= 3
        && !(p.len() == 1 && COMMON.contains(&p[0].as_str()))
}

/// Häufige kurze Wörter, die mit dem Wake-Word konkurrieren (nur die, die das Modell kennt, landen in der Grammatik).
const COMMON: &[&str] = &[
    "ja", "nein", "und", "die", "der", "das", "ist", "ich", "du", "wir", "mit", "es", "nicht",
    "ein", "eine", "hallo", "hey", "okay", "bitte", "danke", "heute", "morgen", "was", "wie", "wo",
    "wann", "gut", "alle", "also", "aber", "oder", "dann", "schon", "noch", "mal", "so", "hier",
    "da", "jetzt", "gleich", "stelle", "stellen", "teller", "welle", "helle", "elle", "bella",
    "ella", "hella", "stall", "still", "stern", "alexa", "siri", "google", "hilfe", "licht",
    "musik", "wetter", "zeit", "uhr", "essen", "mama", "papa", "nach", "auf", "aus", "an", "zu",
    "in",
];

/// Lockwörter zum Wake-Word: andere Anlaute und Endungen („stella“ → „bella“, „ella“, „stelle“ …) plus häufige Wörter.
pub fn decoys(phrase: &[String]) -> Vec<String> {
    const ONSETS: &[&str] = &[
        "", "b", "d", "f", "g", "h", "k", "l", "m", "n", "p", "r", "s", "t", "w", "z", "sch", "st",
        "sp", "tr", "br",
    ];
    const ENDS: &[&str] = &["a", "e", "i", "o", "en", "er"];
    let mut out: Vec<String> = COMMON.iter().map(|w| w.to_string()).collect();
    for w in phrase {
        let start = w.find(|c: char| "aeiouäöüy".contains(c)).unwrap_or(0);
        let rest = &w[start..];
        if rest.chars().count() >= 2 {
            out.extend(ONSETS.iter().map(|o| format!("{o}{rest}")));
        }
        if let Some((cut, _)) = w
            .char_indices()
            .last()
            .filter(|(_, c)| "aeiouy".contains(*c))
        {
            out.extend(ENDS.iter().map(|e| format!("{}{e}", &w[..cut])));
        }
    }
    out.sort();
    out.dedup();
    out.retain(|d| !phrase.contains(d));
    out
}

/// Wie `matches`, gibt aber die Position des letzten Worts der Phrase zurück.
pub fn find(found: &[(String, f32)], phrase: &[String], min_conf: f32) -> Option<usize> {
    if phrase.is_empty() || found.len() < phrase.len() {
        return None;
    }
    found
        .windows(phrase.len())
        .position(|win| {
            win.iter()
                .zip(phrase)
                .all(|((w, c), p)| w == p && *c >= min_conf)
        })
        .map(|i| i + phrase.len() - 1)
}

/// Kommt die Phrase in den erkannten Wörtern vor, jedes Wort mit mindestens `min_conf`?
#[cfg(test)]
pub fn matches(found: &[(String, f32)], phrase: &[String], min_conf: f32) -> bool {
    find(found, phrase, min_conf).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(v: &[(&str, f32)]) -> Vec<(String, f32)> {
        v.iter().map(|(w, c)| (w.to_string(), *c)).collect()
    }

    #[test]
    fn single_word() {
        let p = vec!["stella".to_string()];
        assert!(matches(&f(&[("stella", 1.0)]), &p, 0.65));
        assert!(matches(&f(&[("[unk]", 0.4), ("stella", 0.9)]), &p, 0.65));
        assert!(!matches(&f(&[("stella", 0.5)]), &p, 0.65));
        assert!(!matches(&f(&[("[unk]", 1.0)]), &p, 0.65));
        assert!(!matches(&[], &p, 0.65));
    }

    #[test]
    fn find_returns_last_word() {
        let p = vec!["hey".to_string(), "computer".to_string()];
        let w = f(&[
            ("[unk]", 1.0),
            ("hey", 0.9),
            ("computer", 0.9),
            ("[unk]", 0.5),
        ]);
        assert_eq!(find(&w, &p, 0.65), Some(2));
        assert_eq!(find(&w, &[], 0.65), None);
    }

    #[test]
    fn audio_after_the_name() {
        let mut d = Detector::new(None, &Wake::default());
        // ohne Modell nimmt feed nichts an – Puffer direkt füllen wie feed es täte
        let total = KEEP + crate::dsp::RATE as usize; // 9 s, die erste Sekunde fällt raus
        d.fed.extend((0..KEEP).map(|i| (i % 1000) as i16));
        d.fed_total = total as u64;
        // Name endet bei 8,5 s → die letzte halbe Sekunde
        let after = d.after(8.5);
        assert_eq!(after.len(), crate::dsp::RATE as usize / 2);
        // Name lag vor dem Puffer → alles
        assert_eq!(d.after(0.2).len(), KEEP);
        // Name ganz am Ende → nichts
        assert!(d.after(9.0).is_empty());
    }

    #[test]
    fn decoys_exclude_the_wake_word() {
        let p = vec!["stella".to_string()];
        let d = decoys(&p);
        assert!(!d.contains(&"stella".to_string()));
        for w in ["bella", "ella", "stelle", "tella", "stello"] {
            assert!(d.contains(&w.to_string()), "{w} fehlt");
        }
        let p = vec!["hey".to_string(), "stella".to_string()];
        assert!(!decoys(&p).contains(&"hey".to_string()));
        assert!(!decoys(&["ok".to_string()]).is_empty());
    }

    #[test]
    fn aliases_count_as_the_name() {
        let w = Wake {
            word: "stella".into(),
            second_word: "computer".into(),
            alias: "Stela, stellar, stela".into(),
            second_alias: "kompjuter".into(),
            ..Default::default()
        };
        let p = w.phrases();
        assert_eq!(p.len(), 5);
        assert!(p.contains(&(0, vec!["stela".to_string()])));
        assert!(p.contains(&(1, vec!["kompjuter".to_string()])));
        assert!(usable_alias(&["stela".to_string()]));
        assert!(!usable_alias(&["ja".to_string()]));
        assert!(!usable_alias(&["die".to_string()]));
        assert!(!usable_alias(&[]));
    }

    #[test]
    fn phrase_needs_order_and_confidence() {
        let p = vec!["hey".to_string(), "stella".to_string()];
        assert!(matches(&f(&[("hey", 0.9), ("stella", 0.8)]), &p, 0.65));
        assert!(!matches(&f(&[("stella", 0.9), ("hey", 0.9)]), &p, 0.65));
        assert!(!matches(&f(&[("hey", 0.4), ("stella", 0.9)]), &p, 0.65));
        assert!(!matches(&f(&[("stella", 1.0)]), &p, 0.65));
    }

    /// Mit echtem Modell: `VOSK_TEST_MODEL=/pfad/zum/modell cargo test -- --ignored`
    #[test]
    #[ignore]
    fn model_knows_default_word() {
        let path = std::env::var("VOSK_TEST_MODEL").expect("VOSK_TEST_MODEL");
        let mut model = Model::new(path).expect("Modell");
        for w in ["stella", "ida", "computer", "hey"] {
            println!("{w}: {}", model.find_word(w).is_some());
        }
        let d = Detector::new(Some(&mut model), &Wake::default());
        assert!(d.ready());
        assert!(d.unknown.is_empty(), "unbekannt: {:?}", d.unknown);
    }

    /// Mit echter Aufnahme (16 kHz mono s16le):
    /// `VOSK_TEST_MODEL=… VOSK_TEST_RAW=aufnahme.raw VOSK_TEST_HITS=1 cargo test -- --ignored`
    #[test]
    #[ignore]
    fn detects_in_recording() {
        let (Ok(path), Ok(raw)) = (
            std::env::var("VOSK_TEST_MODEL"),
            std::env::var("VOSK_TEST_RAW"),
        ) else {
            return;
        };
        let want: usize = std::env::var("VOSK_TEST_HITS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        let word = std::env::var("VOSK_TEST_WORD").unwrap_or_else(|_| "stella".into());
        let mut model = Model::new(path).expect("Modell");
        let bytes = std::fs::read(raw).expect("Aufnahme");
        let pcm: Vec<i16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b))
            .collect();
        let mut d = Detector::new(
            Some(&mut model),
            &Wake {
                word,
                ..Default::default()
            },
        );
        let mut hits = 0;
        for chunk in pcm.chunks(800).chain(std::iter::once(&[0i16; 16000][..])) {
            if d.feed(chunk).is_some() {
                hits += 1;
                d.reset();
            }
        }
        println!("Treffer: {hits}, zuletzt gehört: {}", d.heard);
        assert_eq!(hits, want);
    }
}
