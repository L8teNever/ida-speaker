//! Mikrofon und Lautsprecher (cpal/ALSA) plus die Verarbeitung: Echo-Filter, Pegel, Wake-Word oder Upload ins Gespräch.
//!
//! Ein eigener Thread besitzt die Streams (cpal-Streams sind nicht `Send`). Ändern sich Geräte oder Filter auf der
//! Website, baut er alles neu auf – ohne Neustart des Containers.

use crate::config::Config;
use crate::dsp::{self, Aec, FRAME, Resampler};
use crate::state::{App, Command, Phase};
use crate::wake::{self, Detector};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, StreamConfig};
use serde::Serialize;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, Serialize)]
pub struct DeviceEntry {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct DeviceList {
    pub inputs: Vec<DeviceEntry>,
    pub outputs: Vec<DeviceEntry>,
}

/// Alle Geräte für die Auswahl auf der Website.
pub fn list_devices() -> DeviceList {
    let host = cpal::default_host();
    let entry = |d: cpal::Device| {
        let id = d.id().ok()?.to_string();
        let name = d
            .description()
            .map(|x| x.name().to_string())
            .unwrap_or_else(|_| id.clone());
        Some(DeviceEntry { id, name })
    };
    DeviceList {
        inputs: host
            .input_devices()
            .map(|it| it.filter_map(entry).collect())
            .unwrap_or_default(),
        outputs: host
            .output_devices()
            .map(|it| it.filter_map(entry).collect())
            .unwrap_or_default(),
    }
}

fn pick(host: &cpal::Host, id: &str, input: bool) -> Option<cpal::Device> {
    if !id.is_empty()
        && let Ok(parsed) = id.parse()
        && let Some(d) = host.device_by_id(&parsed)
    {
        return Some(d);
    }
    if input {
        host.default_input_device()
    } else {
        host.default_output_device()
    }
}

fn name_of(d: &cpal::Device) -> String {
    d.description()
        .map(|x| x.name().to_string())
        .or_else(|_| d.id().map(|i| i.to_string()))
        .unwrap_or_default()
}

/// Startet den Audio-Thread; er läuft für immer und baut sich bei Fehlern oder Änderungen neu auf.
pub fn spawn(app: Arc<App>) {
    std::thread::Builder::new()
        .name("audio".into())
        .spawn(move || {
            let mut model = wake::load_model(&app.data);
            if model.is_none() {
                app.log(
                    "Kein Vosk-Modell gefunden – Wake-Word aus, Gespräch nur über die Website.",
                );
            }
            loop {
                if let Err(e) = run(&app, &mut model) {
                    app.log(format!("Audio: {e:#}"));
                    app.status.lock().unwrap().audio.error = format!("{e:#}");
                    if !app.talking.load(Ordering::Relaxed) {
                        app.set_phase(Phase::Error);
                    }
                    std::thread::sleep(Duration::from_secs(3));
                }
            }
        })
        .expect("audio thread");
}

/// Teile der Einstellungen, deren Änderung Streams oder Filter neu aufbaut.
fn rebuild_key(c: &Config) -> (String, String, String, bool, u32, crate::config::Echo) {
    (
        c.audio.input.clone(),
        c.audio.output.clone(),
        c.audio.bluetooth.clone(),
        c.audio.buffer,
        c.audio.buffer_ms,
        c.echo.clone(),
    )
}

fn run(app: &Arc<App>, model: &mut Option<vosk::Model>) -> anyhow::Result<()> {
    let mut cfg_rx = app.config.subscribe();
    let cfg = cfg_rx.borrow_and_update().clone();
    let host = cpal::default_host();
    let input = pick(&host, &cfg.audio.input, true)
        .ok_or_else(|| anyhow::anyhow!("kein Mikrofon gefunden"))?;
    let mut output = pick(&host, &cfg.audio.output, false)
        .ok_or_else(|| anyhow::anyhow!("kein Lautsprecher gefunden"))?;
    // Bluetooth-Box gewählt, aber gerade nicht da: über das Standardgerät spielen, bis sie wieder verbunden ist
    let bt = cfg.audio.output == crate::bluetooth::OUTPUT_ID;
    if bt
        && !app.status.lock().unwrap().bluetooth.connected
        && let Some(d) = host.default_output_device()
    {
        output = d;
    }

    let in_cfg = input.default_input_config()?;
    let in_rate = in_cfg.sample_rate();
    let volume = Arc::new(AtomicU32::new(cfg.audio.volume));
    let reference: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
    let open = |dev: cpal::Device| -> anyhow::Result<(cpal::Device, u32, cpal::Stream)> {
        let c = dev.default_output_config()?;
        let rate = c.sample_rate();
        let fmt = c.sample_format();
        let supported = *c.buffer_size();
        let mut sc: StreamConfig = c.into();
        // Eigener Puffer, sonst wählt z. B. bluez-alsa Sekunden und jeder Ton kommt spät
        if cfg.audio.buffer {
            let frames = period_frames(rate, cfg.audio.buffer_ms, &supported);
            sc.buffer_size = cpal::BufferSize::Fixed(frames);
            match build_output(
                &dev,
                fmt,
                sc,
                app.clone(),
                volume.clone(),
                reference.clone(),
            ) {
                Ok(stream) => return Ok((dev, rate, stream)),
                Err(e) => app.log(format!(
                    "Lautsprecher: Puffer {} ms geht nicht ({e:#}) – nehme den Standard des Geräts",
                    cfg.audio.buffer_ms
                )),
            }
        }
        sc.buffer_size = cpal::BufferSize::Default;
        let stream = build_output(
            &dev,
            fmt,
            sc,
            app.clone(),
            volume.clone(),
            reference.clone(),
        )?;
        Ok((dev, rate, stream))
    };
    // Bluetooth-Box lässt sich nicht öffnen (bluez-alsa fehlt, Box weg …): über das Standardgerät spielen
    let (output, out_rate, out_stream) = match open(output) {
        Ok(x) => x,
        Err(e) if bt => {
            app.log(format!(
                "Bluetooth-Lautsprecher lässt sich nicht öffnen ({e:#}) – nehme das Standardgerät"
            ));
            open(
                host.default_output_device()
                    .ok_or_else(|| anyhow::anyhow!("kein Lautsprecher gefunden"))?,
            )?
        }
        Err(e) => return Err(e),
    };
    {
        let mut p = app.playback.lock().unwrap();
        p.queue.clear();
        p.music.clear();
        p.rate = out_rate;
        let buffer = if cfg.audio.buffer {
            cfg.audio.buffer_ms
        } else {
            0
        };
        p.latency_ms = u64::from(buffer + cfg.echo.delay_ms);
    }

    let (tx, rx) = mpsc::sync_channel::<Vec<f32>>(64);
    let in_stream = build_input(&input, in_cfg.sample_format(), in_cfg.into(), tx)?;
    in_stream.play()?;
    out_stream.play()?;

    let aec = Aec::new(&cfg.echo);
    let mut detector = Detector::new(model.as_mut(), &cfg.wake);
    {
        let mut s = app.status.lock().unwrap();
        s.audio.input = name_of(&input);
        s.audio.output = name_of(&output);
        s.audio.input_rate = in_rate;
        s.audio.output_rate = out_rate;
        s.audio.echo_filter = aec.active() && cfg.echo.enabled;
        s.audio.model = detector.ready();
        s.audio.unknown_words = detector.unknown.clone();
        s.audio.error.clear();
    }
    app.log(format!(
        "Audio läuft: Mikrofon „{}“ ({in_rate} Hz), Lautsprecher „{}“ ({out_rate} Hz)",
        name_of(&input),
        name_of(&output)
    ));
    if !detector.unknown.is_empty() {
        app.log(format!(
            "Das Sprachmodell kennt diese Wörter nicht: {}",
            detector.unknown.join(", ")
        ));
    }
    if !app.talking.load(Ordering::Relaxed) {
        app.set_phase(app.idle_phase());
    }

    let mut mic_rs = Resampler::new(in_rate, dsp::RATE);
    let mut ref_rs = Resampler::new(out_rate, dsp::RATE);
    let mut mic_buf: Vec<f32> = Vec::with_capacity(4 * FRAME);
    // Referenz um die eingestellte Verzögerung später an den Echo-Filter geben, damit sie zum Echo im Mikrofon passt
    let ref_delay = (dsp::RATE * cfg.echo.delay_ms / 1000) as usize;
    let mut ref_buf: Vec<f32> = vec![0.0; ref_delay];
    let mut wake_buf: Vec<i16> = Vec::with_capacity(1600);
    let mut gain = cfg.audio.mic_gain as f32 / 100.0;
    let mut mute_while_speaking = cfg.echo.mute_while_speaking;
    let mut wake_on = cfg.wake.enabled;
    let mut second_word = cfg.wake.second_word.clone();
    let key = rebuild_key(&cfg);
    let mut peak = 0u32;
    let mut frames = 0u32;

    loop {
        if app.rebuild_audio.swap(false, Ordering::Relaxed) {
            return Ok(());
        }
        if cfg_rx.has_changed().unwrap_or(false) {
            let next = cfg_rx.borrow_and_update().clone();
            if rebuild_key(&next) != key {
                app.log("Audio-Einstellungen geändert – starte Audio neu");
                return Ok(());
            }
            volume.store(next.audio.volume, Ordering::Relaxed);
            gain = next.audio.mic_gain as f32 / 100.0;
            mute_while_speaking = next.echo.mute_while_speaking;
            wake_on = next.wake.enabled;
            second_word = next.wake.second_word.clone();
            detector = Detector::new(model.as_mut(), &next.wake);
            let mut s = app.status.lock().unwrap();
            s.audio.model = detector.ready();
            s.audio.unknown_words = detector.unknown.clone();
        }

        let chunk = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(c) => c,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => anyhow::bail!("Mikrofon-Stream beendet"),
        };
        if gain != 1.0 {
            let scaled: Vec<f32> = chunk.iter().map(|s| (s * gain).clamp(-1.0, 1.0)).collect();
            mic_rs.process(&scaled, &mut mic_buf);
        } else {
            mic_rs.process(&chunk, &mut mic_buf);
        }
        let played = std::mem::take(&mut *reference.lock().unwrap());
        // Kalibrierung: Messton im Referenzsignal erkennen, bevor ein Rückstand unten verworfen wird
        if let Some(c) = app.calib.lock().unwrap().as_mut()
            && c.ref_at.is_none()
            && played
                .chunks(FRAME * 3)
                .any(|b| dsp::rms(b) > CALIB_REF_LEVEL)
        {
            c.ref_at = Some(c.frame);
        }
        ref_rs.process(&played, &mut ref_buf);
        // Mehr als eine halbe Sekunde Rückstand beim Referenzsignal: verwerfen statt immer weiter auflaufen lassen
        if ref_buf.len() > ref_delay + dsp::RATE as usize / 2 {
            let drop = ref_buf.len() - ref_delay - FRAME * 4;
            ref_buf.drain(..drop);
        }

        while mic_buf.len() >= FRAME {
            // Erst, was der Lautsprecher gespielt hat, dann das Mikrofon – so findet AEC3 das Echo
            let mut cal = app.calib.lock().unwrap();
            while ref_buf.len() >= FRAME {
                let mut r: Vec<f32> = ref_buf.drain(..FRAME).collect();
                aec.render(&mut r);
            }
            let mut frame: Vec<f32> = mic_buf.drain(..FRAME).collect();
            // Messung am rohen Mikrofon – der Echo-Filter würde den Ton ja gerade herausrechnen
            if let Some(c) = cal.as_mut()
                && let Some(result) = calib_step(c, dsp::rms(&frame) as f32)
            {
                if let Some(tx) = c.done.take() {
                    let _ = tx.send(result);
                }
                *cal = None;
            }
            drop(cal);
            aec.capture(&mut frame);
            let level = dsp::rms(&frame);
            peak = peak.max(level);
            frames += 1;
            if frames >= 5 {
                app.level.store(peak, Ordering::Relaxed);
                peak = 0;
                frames = 0;
            }

            let pcm: Vec<i16> = frame.iter().map(|s| dsp::f32_to_i16(*s)).collect();
            if app.talking.load(Ordering::Relaxed) {
                let silent = mute_while_speaking && app.playing();
                let out = if silent { vec![0i16; pcm.len()] } else { pcm };
                if let Some(tx) = app.mic.lock().unwrap().as_ref() {
                    let _ = tx.try_send(out);
                }
                wake_buf.clear();
                continue;
            }
            if learning(app, model.as_ref(), &pcm) {
                wake_buf.clear();
                continue;
            }
            if !wake_on || app.muted() || !app.present.load(Ordering::Relaxed) || app.playing() {
                wake_buf.clear();
                continue;
            }
            wake_buf.extend_from_slice(&pcm);
            if wake_buf.len() >= 800 {
                if let Some(hit) = detector.feed(&wake_buf) {
                    detector.reset();
                    app.status.lock().unwrap().audio.heard = detector.heard.clone();
                    let quick = hit.which == 1;
                    let source = if quick {
                        format!("Wake-Word „{}“", second_word)
                    } else {
                        "Wake-Word".into()
                    };
                    let _ = app.commands.send(Command::Start {
                        source,
                        quick,
                        pre: if quick { hit.after } else { Vec::new() },
                    });
                } else if !detector.heard.is_empty() {
                    app.status.lock().unwrap().audio.heard = std::mem::take(&mut detector.heard);
                }
                wake_buf.clear();
            }
        }
    }
}

/// Ab diesem Pegel (RMS, i16-Skala) gilt der Messton im Referenzsignal als gestartet.
const CALIB_REF_LEVEL: u32 = 1500;
/// So lange wird höchstens gemessen (in 10-ms-Blöcken, 12 s – ohne eigenen Puffer kann es Sekunden dauern).
const CALIB_MAX_FRAMES: u64 = 1200;

/// Ein Mikrofon-Block der Kalibrierung. `Some(Some(n))` = Ton nach n Blöcken gehört, `Some(None)` = nichts gehört, `None` = weiter.
fn calib_step(c: &mut crate::state::Calib, level: f32) -> Option<Option<i64>> {
    c.frame += 1;
    match c.ref_at {
        // Vor dem Ton: Grundrauschen lernen
        None => {
            c.noise = if c.frame == 1 {
                level
            } else {
                c.noise * 0.9 + level * 0.1
            }
        }
        Some(at) if level > (c.noise * 4.0).max(300.0) => {
            c.mic_at = Some(c.frame);
            return Some(Some(c.frame as i64 - at as i64));
        }
        Some(_) => {}
    }
    (c.frame > CALIB_MAX_FRAMES).then_some(None)
}

/// „Namen einsprechen“: sammelt das Mikrofon (erst nach dem Signalton) und lässt Vosk frei erkennen. `true` = Aufnahme läuft.
fn learning(app: &App, model: Option<&vosk::Model>, pcm: &[i16]) -> bool {
    let mut guard = app.learn.lock().unwrap();
    let Some(l) = guard.as_mut() else {
        return false;
    };
    if app.playing() {
        return true;
    }
    l.buf.extend_from_slice(pcm);
    if l.buf.len() < l.need {
        return true;
    }
    let mut l = guard.take().expect("Aufnahme");
    drop(guard);
    let text = model
        .map(|m| wake::transcribe(m, &l.buf))
        .unwrap_or_default();
    if let Some(tx) = l.done.take() {
        let _ = tx.send(text);
    }
    true
}

fn build_input(
    device: &cpal::Device,
    fmt: SampleFormat,
    config: StreamConfig,
    tx: mpsc::SyncSender<Vec<f32>>,
) -> anyhow::Result<cpal::Stream> {
    macro_rules! go {
        ($t:ty) => {
            input_stream::<$t>(device, config, tx)
        };
    }
    match fmt {
        SampleFormat::I16 => go!(i16),
        SampleFormat::I32 => go!(i32),
        SampleFormat::U16 => go!(u16),
        SampleFormat::U8 => go!(u8),
        SampleFormat::I8 => go!(i8),
        SampleFormat::F32 => go!(f32),
        SampleFormat::F64 => go!(f64),
        other => anyhow::bail!("Mikrofon-Format {other} wird nicht unterstützt"),
    }
}

fn input_stream<T>(
    device: &cpal::Device,
    config: StreamConfig,
    tx: mpsc::SyncSender<Vec<f32>>,
) -> anyhow::Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let ch = config.channels.max(1) as usize;
    let stream = device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            let mono: Vec<f32> = data
                .chunks(ch)
                .map(|f| f.iter().map(|s| f32::from_sample(*s)).sum::<f32>() / ch as f32)
                .collect();
            let _ = tx.try_send(mono);
        },
        |e| tracing::warn!("Mikrofon: {e}"),
        None,
    )?;
    Ok(stream)
}

/// Periodengröße in Frames für den gewünschten Puffer (cpal nimmt 2 Perioden), im erlaubten Bereich des Geräts.
fn period_frames(rate: u32, buffer_ms: u32, supported: &cpal::SupportedBufferSize) -> u32 {
    let want = (u64::from(rate) * u64::from(buffer_ms) / 2000).max(64) as u32;
    match *supported {
        cpal::SupportedBufferSize::Range { min, max } if min <= max => want.clamp(min, max),
        _ => want,
    }
}

fn build_output(
    device: &cpal::Device,
    fmt: SampleFormat,
    config: StreamConfig,
    app: Arc<App>,
    volume: Arc<AtomicU32>,
    reference: Arc<Mutex<Vec<f32>>>,
) -> anyhow::Result<cpal::Stream> {
    macro_rules! go {
        ($t:ty) => {
            output_stream::<$t>(device, config, app, volume, reference)
        };
    }
    match fmt {
        SampleFormat::I16 => go!(i16),
        SampleFormat::I32 => go!(i32),
        SampleFormat::U16 => go!(u16),
        SampleFormat::U8 => go!(u8),
        SampleFormat::I8 => go!(i8),
        SampleFormat::F32 => go!(f32),
        SampleFormat::F64 => go!(f64),
        other => anyhow::bail!("Lautsprecher-Format {other} wird nicht unterstützt"),
    }
}

fn output_stream<T>(
    device: &cpal::Device,
    config: StreamConfig,
    app: Arc<App>,
    volume: Arc<AtomicU32>,
    reference: Arc<Mutex<Vec<f32>>>,
) -> anyhow::Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let ch = config.channels.max(1) as usize;
    // Musik weich leiser/lauter (ca. 150 ms), damit es beim Wake-Word nicht knackt
    let step = 1.0 / (config.sample_rate as f32 * 0.15).max(1.0);
    let mut music_gain = 1.0f32;
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            let vol = volume.load(Ordering::Relaxed) as f32 / 100.0;
            let mut played = Vec::with_capacity(data.len() / ch);
            {
                let mut p = app.playback.lock().unwrap();
                let duck = app.ducking.load(Ordering::Relaxed) || !p.queue.is_empty();
                let target = if duck {
                    app.duck_pct.load(Ordering::Relaxed).min(100) as f32 / 100.0
                } else {
                    1.0
                };
                for frame in data.chunks_mut(ch) {
                    music_gain = duck_step(music_gain, target, step);
                    let music = p.music.pop_front().unwrap_or(0.0) * music_gain;
                    let voice = p.queue.pop_front().unwrap_or(0.0);
                    let v = ((voice + music) * vol).clamp(-1.0, 1.0);
                    played.push(v);
                    let s = T::from_sample(v);
                    frame.iter_mut().for_each(|x| *x = s);
                }
            }
            // Referenz für den Echo-Filter: genau das, was gerade rausgeht (begrenzt, falls niemand abholt)
            let mut r = reference.lock().unwrap();
            if r.len() < 48_000 {
                r.extend_from_slice(&played);
            }
        },
        |e| tracing::warn!("Lautsprecher: {e}"),
        None,
    )?;
    Ok(stream)
}

/// Ein Sample näher an die Ziel-Lautstärke der Musik.
fn duck_step(gain: f32, target: f32, step: f32) -> f32 {
    if gain > target {
        (gain - step).max(target)
    } else {
        (gain + step).min(target)
    }
}

/// Rechnet Stellas 24-kHz-Stimme auf die Rate des Lautsprechers um.
pub struct VoiceOut {
    rs: Resampler,
    rate: u32,
}

impl VoiceOut {
    pub fn new(out_rate: u32) -> Self {
        Self {
            rs: Resampler::new(dsp::VOICE_RATE, out_rate.max(8000)),
            rate: out_rate,
        }
    }

    pub fn push(&mut self, app: &App, pcm_le: &[u8]) {
        if app.output_rate() != self.rate && app.output_rate() > 0 {
            *self = Self::new(app.output_rate());
        }
        let input: Vec<f32> = pcm_le
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| dsp::i16_to_f32(i16::from_le_bytes(*b)))
            .collect();
        let mut out = Vec::with_capacity(input.len() * 2);
        self.rs.process(&input, &mut out);
        app.play(&out);
    }
}

/// Signaltöne: „ich höre zu“ (aufsteigend) und „Gespräch zu Ende“ (absteigend).
pub fn beep(app: &App, kind: Beep) {
    let rate = app.output_rate().max(8000);
    let notes: &[(f32, u32)] = match kind {
        Beep::Listen => &[(660.0, 70), (990.0, 110)],
        Beep::End => &[(880.0, 70), (587.0, 120)],
        Beep::Error => &[(330.0, 160), (0.0, 60), (330.0, 160)],
        Beep::Test => &[(523.0, 150), (659.0, 150), (784.0, 250)],
        Beep::Calibrate => &[(1000.0, 250)],
        Beep::Alarm => &[
            (880.0, 120),
            (0.0, 80),
            (880.0, 120),
            (0.0, 80),
            (1175.0, 220),
        ],
    };
    app.play(&dsp::tone(rate, notes, 0.25));
}

#[derive(Debug, Clone, Copy)]
pub enum Beep {
    Listen,
    End,
    Error,
    Test,
    /// Kurzer, klarer Messton für die Kalibrierung.
    Calibrate,
    /// Timer/Wecker klingelt (wird wiederholt).
    Alarm,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kalibrierung_zaehlt_bloecke() {
        let mut c = crate::state::Calib::default();
        for _ in 0..20 {
            assert_eq!(calib_step(&mut c, 50.0), None);
        }
        c.ref_at = Some(c.frame);
        // 25 Blöcke leise, dann kommt der Ton im Mikrofon an
        for _ in 0..24 {
            assert_eq!(calib_step(&mut c, 60.0), None);
        }
        assert_eq!(calib_step(&mut c, 4000.0), Some(Some(25)));
        // ohne Ton: irgendwann aufgeben
        let mut c = crate::state::Calib::default();
        let mut last = None;
        for _ in 0..=CALIB_MAX_FRAMES {
            last = calib_step(&mut c, 50.0);
        }
        assert_eq!(last, Some(None));
    }

    #[test]
    fn musik_wird_weich_leiser() {
        let mut g = 1.0;
        for _ in 0..5 {
            g = duck_step(g, 0.15, 0.25);
        }
        assert_eq!(g, 0.15);
        g = duck_step(g, 1.0, 0.25);
        assert!((g - 0.4).abs() < 1e-6);
    }

    #[test]
    fn puffer_in_frames() {
        let any = cpal::SupportedBufferSize::Unknown;
        assert_eq!(period_frames(48_000, 100, &any), 2400);
        let range = cpal::SupportedBufferSize::Range {
            min: 1024,
            max: 4096,
        };
        assert_eq!(period_frames(48_000, 20, &range), 1024);
        assert_eq!(period_frames(48_000, 1000, &range), 4096);
    }
}
