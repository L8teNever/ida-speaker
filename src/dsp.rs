//! Signalverarbeitung: Abtastraten umrechnen, Pegel messen und der WebRTC-Echo-Filter (AEC3).

use crate::config::{Echo, Level};
use webrtc_audio_processing::Processor;
use webrtc_audio_processing_config as apc;

/// Interne Abtastrate für Wake-Word, Echo-Filter und den Upload zu IDA.
pub const RATE: u32 = 16_000;
/// Stellas Stimme kommt mit 24 kHz.
pub const VOICE_RATE: u32 = 24_000;
/// 10-ms-Block des Echo-Filters bei 16 kHz.
pub const FRAME: usize = (RATE / 100) as usize;

/// Linearer Resampler für Sprache, blockweise ohne Sprünge an den Blockgrenzen.
#[derive(Debug, Clone)]
pub struct Resampler {
    step: f64,
    pos: f64,
    prev: f32,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        Self {
            step: from as f64 / to as f64,
            pos: 0.0,
            prev: 0.0,
        }
    }

    pub fn is_identity(&self) -> bool {
        (self.step - 1.0).abs() < f64::EPSILON
    }

    /// `pos` läuft relativ zum Blockanfang; -1 entspricht dem letzten Wert des vorigen Blocks.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.is_identity() {
            out.extend_from_slice(input);
            return;
        }
        let n = input.len() as f64;
        let at = |i: isize| if i < 0 { self.prev } else { input[i as usize] };
        while self.pos < n - 1.0 + 1e-9 && !input.is_empty() {
            let i = self.pos.floor();
            let frac = (self.pos - i) as f32;
            let i = i as isize;
            let a = at(i);
            let b = if (i + 1) < input.len() as isize {
                at(i + 1)
            } else {
                a
            };
            out.push(a + (b - a) * frac);
            self.pos += self.step;
        }
        if let Some(&last) = input.last() {
            self.prev = last;
        }
        self.pos -= n;
    }
}

pub fn i16_to_f32(s: i16) -> f32 {
    s as f32 / 32768.0
}

pub fn f32_to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

/// RMS auf der Skala von i16 (0–32767).
pub fn rms(samples: &[f32]) -> u32 {
    if samples.is_empty() {
        return 0;
    }
    let sum: f32 = samples.iter().map(|s| s * s).sum();
    ((sum / samples.len() as f32).sqrt() * 32767.0) as u32
}

pub fn rms_i16(samples: &[i16]) -> u32 {
    if samples.is_empty() {
        return 0;
    }
    let sum: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    (sum / samples.len() as f64).sqrt() as u32
}

/// Echo-Filter, Rauschunterdrückung und automatische Lautstärke in einem.
/// `render` bekommt, was gerade aus dem Lautsprecher kommt, `capture` das Mikrofon – beides in 10-ms-Blöcken bei 16 kHz.
pub struct Aec {
    proc: Option<Processor>,
}

impl Aec {
    pub fn new(cfg: &Echo) -> Self {
        let needs = cfg.enabled || cfg.noise != Level::Off || cfg.agc;
        if !needs {
            return Self { proc: None };
        }
        match Processor::new(RATE) {
            Ok(p) => {
                p.set_config(apc::Config {
                    echo_canceller: cfg.enabled.then(apc::EchoCanceller::default),
                    noise_suppression: match cfg.noise {
                        Level::Off => None,
                        Level::Low => Some(apc::NoiseSuppression {
                            level: apc::NoiseSuppressionLevel::Low,
                            ..Default::default()
                        }),
                        Level::Moderate => Some(apc::NoiseSuppression::default()),
                        Level::High => Some(apc::NoiseSuppression {
                            level: apc::NoiseSuppressionLevel::High,
                            ..Default::default()
                        }),
                    },
                    high_pass_filter: Some(apc::HighPassFilter::default()),
                    gain_controller: cfg.agc.then(|| {
                        apc::GainController::GainController2(apc::GainController2::default())
                    }),
                    ..Default::default()
                });
                Self { proc: Some(p) }
            }
            Err(e) => {
                tracing::error!("Echo-Filter nicht verfügbar: {e}");
                Self { proc: None }
            }
        }
    }

    pub fn active(&self) -> bool {
        self.proc.is_some()
    }

    pub fn render(&self, frame: &mut [f32]) {
        if let Some(p) = &self.proc
            && let Err(e) = p.process_render_frame([frame])
        {
            tracing::debug!("render: {e}");
        }
    }

    pub fn capture(&self, frame: &mut [f32]) {
        if let Some(p) = &self.proc
            && let Err(e) = p.process_capture_frame([frame])
        {
            tracing::debug!("capture: {e}");
        }
    }
}

/// Kurzer Signalton (Sinus mit weichem Ein- und Ausblenden).
pub fn tone(rate: u32, notes: &[(f32, u32)], gain: f32) -> Vec<f32> {
    let mut out = Vec::new();
    for &(freq, ms) in notes {
        let n = (rate as u64 * ms as u64 / 1000) as usize;
        let fade = (rate as usize / 200).min(n / 2).max(1);
        for i in 0..n {
            let env = (i.min(n - 1 - i) as f32 / fade as f32).min(1.0);
            let v = (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin();
            out.push(v * env * gain);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_lengths_and_continuity() {
        let mut r = Resampler::new(48_000, 16_000);
        let input: Vec<f32> = (0..4800).map(|i| (i as f32 * 0.001).sin()).collect();
        let mut out = Vec::new();
        for chunk in input.chunks(441) {
            r.process(chunk, &mut out);
        }
        assert!((out.len() as i64 - 1600).abs() <= 2, "{}", out.len());
        // Ein langsamer Sinus bleibt glatt, auch an den Blockgrenzen
        for w in out.windows(2) {
            assert!((w[1] - w[0]).abs() < 0.01);
        }
    }

    #[test]
    fn upsample_24k_to_48k() {
        let mut r = Resampler::new(24_000, 48_000);
        let mut out = Vec::new();
        for chunk in vec![0.5f32; 2400].chunks(100) {
            r.process(chunk, &mut out);
        }
        assert!((out.len() as i64 - 4800).abs() <= 2);
        assert!(out[10..].iter().all(|v| (v - 0.5).abs() < 1e-6));
    }

    #[test]
    fn identity_passes_through() {
        let mut r = Resampler::new(16_000, 16_000);
        let mut out = Vec::new();
        r.process(&[0.1, 0.2], &mut out);
        assert_eq!(out, vec![0.1, 0.2]);
    }

    #[test]
    fn conversions_and_rms() {
        assert_eq!(f32_to_i16(2.0), 32767);
        assert_eq!(f32_to_i16(-2.0), -32767);
        assert!((i16_to_f32(16384) - 0.5).abs() < 1e-6);
        assert_eq!(rms(&[]), 0);
        assert!((rms(&[0.5, -0.5]) as i32 - 16383).abs() <= 1);
    }

    #[test]
    fn tone_has_expected_length() {
        let t = tone(16_000, &[(880.0, 100), (660.0, 50)], 0.3);
        assert_eq!(t.len(), 1600 + 800);
        assert!(t.iter().all(|v| v.abs() <= 0.3 + 1e-6));
    }
}
