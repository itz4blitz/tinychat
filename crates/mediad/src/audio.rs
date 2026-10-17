//! The microphone and the speaker. Everything above this file sees 48 kHz stereo `i16`; a device
//! that runs at another rate or channel count is converted here, without touching its settings.
//!
//! `TINYCLIENT_MEDIAD_AUDIO` replaces the devices for tests: `null` hears silence, `tone` hears a
//! 440 Hz tone, `onset` hears that tone once a second, each time led in by 40 ms too quiet to
//! count as speech, and none of them opens a device. `TINYCLIENT_MEDIAD_SINK` names a file that then takes
//! what would have been played, as raw 48 kHz stereo `i16`.

use crate::{CHANNELS, FRAME_SAMPLES, RATE};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Device-rate stereo samples the callbacks and the media loop pass each other.
type Ring = Arc<Mutex<VecDeque<f32>>>;

/// What is waiting to be played, and whether playing has begun.
#[derive(Default)]
struct Playout {
    ring: VecDeque<f32>,
    /// Enough is held to play. False at the start and after the ring ran dry: the speaker is
    /// given silence until [`PREBUFFER`] is held again, instead of a frame, a gap and a click.
    playing: bool,
    /// Samples still to be faded in after playing (re)started.
    fade: usize,
    underruns: u64,
    /// Play what is held even when it is less than the cushion: a file's last moments.
    flush: bool,
}

impl Playout {
    /// One stereo frame for the device, or silence while filling.
    fn next(&mut self, fill: usize, fade: usize) -> (f32, f32) {
        if !self.playing {
            if self.ring.len() < fill && !(self.flush && !self.ring.is_empty()) {
                return (0., 0.);
            }
            self.playing = true;
            self.fade = fade;
        }
        let frames_left = self.ring.len() / CHANNELS;
        let (Some(left), Some(right)) = (self.ring.pop_front(), self.ring.pop_front()) else {
            // Ran dry: we fell behind the device. Fill again before playing on.
            self.playing = false;
            self.underruns += 1;
            return (0., 0.);
        };
        // Fade in at the start, and fade the last `fade` frames down to silence. A step from a
        // loud sample to zero is the click both ends hear when the cushion runs out.
        let mut gain = 1.;
        if self.fade > 0 && fade > 0 {
            gain *= 1. - self.fade as f32 / fade as f32;
            self.fade -= 1;
        }
        if fade > 0 && frames_left <= fade {
            gain *= frames_left.saturating_sub(1) as f32 / fade as f32;
        }
        (left * gain, right * gain)
    }

    /// Drops samples off the front until `keep` remain. The samples that were about to play are
    /// blended into the new head, so the clock-drift skip is a ramp and not a splice.
    fn trim_front(&mut self, keep: usize, fade_frames: usize) {
        let len = self.ring.len();
        if len <= keep {
            return;
        }
        let mut drop = len - keep;
        drop -= drop % CHANNELS;
        if drop == 0 {
            return;
        }
        let fade = fade_frames
            .min(drop / CHANNELS)
            .min((len - drop) / CHANNELS);
        for i in 0..fade {
            let toward_new = (i + 1) as f32 / (fade as f32 + 1.);
            for channel in 0..CHANNELS {
                let at = i * CHANNELS + channel;
                let kept = self.ring[at];
                let ahead = self.ring[drop + at];
                self.ring[drop + at] = kept * (1. - toward_new) + ahead * toward_new;
            }
        }
        self.ring.drain(..drop);
    }
}

/// Seconds of audio either ring may hold before the oldest is dropped: the device clock and ours
/// drift, and a backlog is delay.
const BACKLOG: f32 = 0.2;
/// Seconds held before the speaker is given anything. The media loop hands over 20 ms at a time
/// and not exactly on time; without a cushion the device runs dry between frames and clicks.
const PREBUFFER: f32 = 0.06;
/// Seconds over which playing fades in when it starts, and out before the cushion runs dry or
/// the ring skips ahead.
const FADE: f32 = 0.005;

#[derive(Debug)]
pub enum AudioError {
    NoMicrophone,
    NoSpeaker,
    /// The microphone exists and would not open. The system's own words, for the log only.
    Microphone(String),
    Speaker(String),
}

/// Every device by name, and the system's defaults.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Listing {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub default_input: String,
    pub default_output: String,
}

fn name(device: &cpal::Device) -> Option<String> {
    device.description().ok().map(|d| d.name().to_owned())
}

/// Asks CoreAudio what exists. Opens nothing.
pub fn listing() -> Listing {
    if let Ok(kind) = std::env::var("TINYCLIENT_MEDIAD_AUDIO") {
        return Listing {
            inputs: vec![kind.clone()],
            outputs: vec!["null".to_owned()],
            default_input: kind,
            default_output: "null".to_owned(),
        };
    }
    let host = cpal::default_host();
    let names = |devices: Option<Vec<cpal::Device>>| -> Vec<String> {
        let mut names: Vec<String> = devices
            .unwrap_or_default()
            .iter()
            .filter_map(name)
            .collect();
        names.dedup();
        names
    };
    Listing {
        inputs: names(host.input_devices().ok().map(Iterator::collect)),
        outputs: names(host.output_devices().ok().map(Iterator::collect)),
        default_input: host
            .default_input_device()
            .as_ref()
            .and_then(name)
            .unwrap_or_default(),
        default_output: host
            .default_output_device()
            .as_ref()
            .and_then(name)
            .unwrap_or_default(),
    }
}

/// Input frames each output sample is weighed from.
const TAPS: usize = 32;
/// Positions between two input frames the filter is tabulated at; between those it is
/// interpolated.
const PHASES: usize = 128;
/// The filter's centre, in frames from its first tap.
const CENTRE: usize = TAPS / 2 - 1;

fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term, mut n) = (1., 1., 1.);
    while term > sum * 1e-12 {
        term *= (x / (2. * n)).powi(2);
        sum += term;
        n += 1.;
    }
    sum
}

/// A windowed-sinc low-pass (Kaiser, about 80 dB down) as a polyphase table: row `p` is the
/// filter for an output that falls `p / PHASES` of the way from one input frame to the next.
/// `cutoff` is a fraction of the input's Nyquist frequency.
fn table(cutoff: f64) -> Vec<[f32; TAPS]> {
    const BETA: f64 = 8.;
    let half = TAPS as f64 / 2.;
    (0..=PHASES)
        .map(|phase| {
            let offset = phase as f64 / PHASES as f64;
            let mut row = [0f64; TAPS];
            for (tap, weight) in row.iter_mut().enumerate() {
                let x = tap as f64 - CENTRE as f64 - offset;
                let sinc = if x.abs() < 1e-9 {
                    1.
                } else {
                    (std::f64::consts::PI * cutoff * x).sin() / (std::f64::consts::PI * cutoff * x)
                };
                let across = (x / half).clamp(-1., 1.);
                let window = bessel_i0(BETA * (1. - across * across).sqrt()) / bessel_i0(BETA);
                *weight = cutoff * sinc * window;
            }
            // Unity gain at every phase, so a steady level does not ripple.
            let sum: f64 = row.iter().sum();
            row.map(|weight| (weight / sum) as f32)
        })
        .collect()
}

/// Band-limited conversion between two rates, one stereo stream: a polyphase windowed sinc. A
/// device at 48 kHz bypasses it. Output sample `n` is the input at time `n * step`; it becomes
/// available `TAPS - CENTRE` input frames later (under half a millisecond).
pub(crate) struct Resampler {
    /// Input frames per output frame.
    step: f64,
    /// Where the next output falls, in frames from the start of `held`.
    position: f64,
    /// Input not yet done with, interleaved. Starts with `CENTRE` frames of silence.
    held: Vec<f32>,
    /// Empty when the rates are equal.
    table: Vec<[f32; TAPS]>,
}

impl Resampler {
    pub(crate) fn new(from: u32, to: u32) -> Resampler {
        let step = f64::from(from) / f64::from(to);
        if from == to {
            return Resampler {
                step,
                position: 0.,
                held: Vec::new(),
                table: Vec::new(),
            };
        }
        // Going down, everything above the new Nyquist frequency has to go first.
        let cutoff = 0.95 * (f64::from(to) / f64::from(from)).min(1.);
        Resampler {
            step,
            position: 0.,
            held: vec![0.; CENTRE * CHANNELS],
            table: table(cutoff),
        }
    }

    pub(crate) fn push(&mut self, input: &[f32], output: &mut Vec<f32>) {
        if self.table.is_empty() {
            output.extend_from_slice(input);
            return;
        }
        self.held
            .extend_from_slice(&input[..input.len() - input.len() % CHANNELS]);
        let frames = self.held.len() / CHANNELS;
        while self.position as usize + TAPS <= frames {
            let first = self.position as usize;
            let between = (self.position - first as f64) * PHASES as f64;
            let phase = between as usize;
            let blend = (between - phase as f64) as f32;
            let (near, far) = (&self.table[phase], &self.table[phase + 1]);
            let mut sums = [0f32; CHANNELS];
            let taps = self.held[first * CHANNELS..(first + TAPS) * CHANNELS]
                .as_chunks::<CHANNELS>()
                .0;
            for (frame, (near, far)) in taps.iter().zip(near.iter().zip(far)) {
                let weight = near + (far - near) * blend;
                for (sum, sample) in sums.iter_mut().zip(frame) {
                    *sum += sample * weight;
                }
            }
            output.extend_from_slice(&sums);
            self.position += self.step;
        }
        let done = (self.position as usize).min(frames);
        self.held.drain(..done * CHANNELS);
        self.position -= done as f64;
    }
}

/// The level of a frame, 0 to 100, on a scale where speech sits in the middle. 0 is under
/// -60 dBFS.
pub fn level(pcm: &[i16]) -> u8 {
    if pcm.is_empty() {
        return 0;
    }
    let mean = pcm.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / pcm.len() as f64;
    let db = 10. * (mean / (32768f64 * 32768.)).max(1e-12).log10();
    ((db + 60.) / 60. * 100.).clamp(0., 100.) as u8
}

struct Input {
    stream: cpal::Stream,
    ring: Ring,
    resampler: Resampler,
    running: bool,
}

struct Output {
    _stream: cpal::Stream,
    playout: Arc<Mutex<Playout>>,
    resampler: Resampler,
    /// Samples the ring may hold, and how many it is cut back to when it is over.
    room: usize,
    fill: usize,
    /// Frames of the ramp used when the ring skips ahead.
    fade: usize,
}

enum Source {
    Silence,
    Tone {
        phase: f64,
    },
    /// A word a second: half a second of nothing, 40 ms of a faint tone, then the tone.
    Onset {
        at: u32,
    },
    Device(Option<Input>),
}

enum Sink {
    Null,
    File(File),
    Device(Option<Output>),
}

pub struct Audio {
    source: Source,
    sink: Sink,
    /// 48 kHz samples captured and not yet taken as a frame.
    captured: Vec<f32>,
    scratch: Vec<f32>,
    /// Set by a device callback when its device went away or changed under it.
    broken: Arc<AtomicBool>,
    /// The devices asked for by name. `None` follows the system default.
    pub want_input: Option<String>,
    pub want_output: Option<String>,
    pub input: String,
    pub output: String,
    /// Which of the two are opened. A call opens both; a test opens only what it tries.
    sides: (bool, bool),
}

/// Devices opened away from the call thread, ready to take over.
pub struct Fresh {
    source: Source,
    sink: Sink,
    broken: Arc<AtomicBool>,
    input: String,
    output: String,
}

/// Streams a call has stopped using. Drop this away from the 20 ms thread: stopping a device
/// is the slow half of a reopen. The fields exist so that drop runs here, not on the tick.
#[allow(dead_code)]
pub struct Retired {
    source: Source,
    sink: Sink,
}

/// True when the system default we follow is no longer the device we have open.
pub(crate) fn route_moved(
    want_input: &Option<String>,
    input: &str,
    want_output: &Option<String>,
    output: &str,
    sides: (bool, bool),
) -> bool {
    let host = cpal::default_host();
    let moved = |wanted: &Option<String>, current: &str, device: Option<cpal::Device>| {
        wanted.is_none()
            && device
                .as_ref()
                .and_then(name)
                .is_some_and(|name| name != current)
    };
    (sides.0 && moved(want_input, input, host.default_input_device()))
        || (sides.1 && moved(want_output, output, host.default_output_device()))
}

/// Opens the wanted devices without touching a call that is already playing.
pub(crate) fn open_fresh(
    want_input: Option<&str>,
    want_output: Option<&str>,
    sides: (bool, bool),
    mic_running: bool,
) -> Result<Fresh, AudioError> {
    let host = cpal::default_host();
    let broken = Arc::new(AtomicBool::new(false));
    let mut source = Source::Device(None);
    let mut sink = Sink::Device(None);
    let mut input_name = String::new();
    let mut output_name = String::new();
    if sides.0 {
        let microphone = find(&host, want_input, true).ok_or(AudioError::NoMicrophone)?;
        let mut input = open_input(&microphone, &broken)?;
        if !mic_running {
            let _ = input.stream.pause();
            input.running = false;
        }
        input_name = name(&microphone).unwrap_or_default();
        source = Source::Device(Some(input));
    }
    if sides.1 {
        let speaker = find(&host, want_output, false).ok_or(AudioError::NoSpeaker)?;
        sink = Sink::Device(Some(open_output(&speaker, &broken)?));
        output_name = name(&speaker).unwrap_or_default();
    }
    Ok(Fresh {
        source,
        sink,
        broken,
        input: input_name,
        output: output_name,
    })
}

fn find(host: &cpal::Host, wanted: Option<&str>, input: bool) -> Option<cpal::Device> {
    if let Some(wanted) = wanted {
        let devices: Option<Vec<cpal::Device>> = if input {
            host.input_devices().ok().map(Iterator::collect)
        } else {
            host.output_devices().ok().map(Iterator::collect)
        };
        if let Some(device) = devices
            .unwrap_or_default()
            .into_iter()
            .find(|d| name(d).as_deref() == Some(wanted))
        {
            return Some(device);
        }
        eprintln!("mediad: no device named {wanted:?}, using the system default");
    }
    if input {
        host.default_input_device()
    } else {
        host.default_output_device()
    }
}

fn open_input(device: &cpal::Device, broken: &Arc<AtomicBool>) -> Result<Input, AudioError> {
    let supported = device
        .default_input_config()
        .map_err(|e| AudioError::Microphone(e.to_string()))?;
    let config = supported.config();
    let (channels, rate) = (usize::from(config.channels), config.sample_rate);
    let ring: Ring = Arc::default();
    let room = (rate as f32 * BACKLOG) as usize * CHANNELS;
    let (filled, flag) = (ring.clone(), broken.clone());
    let stream = device
        .build_input_stream(
            config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                let mut ring = filled.lock().unwrap();
                for frame in data.chunks_exact(channels) {
                    // Mono is heard in both ears; of more than two channels the first two count.
                    ring.push_back(frame[0]);
                    ring.push_back(frame[if channels > 1 { 1 } else { 0 }]);
                }
                let over = ring.len().saturating_sub(room);
                ring.drain(..over);
            },
            move |err| {
                eprintln!("mediad: microphone: {err}");
                flag.store(true, Ordering::Relaxed);
            },
            None,
        )
        .map_err(|e| AudioError::Microphone(e.to_string()))?;
    stream
        .play()
        .map_err(|e| AudioError::Microphone(e.to_string()))?;
    Ok(Input {
        stream,
        ring,
        resampler: Resampler::new(rate, RATE),
        running: true,
    })
}

fn open_output(device: &cpal::Device, broken: &Arc<AtomicBool>) -> Result<Output, AudioError> {
    let supported = device
        .default_output_config()
        .map_err(|e| AudioError::Speaker(e.to_string()))?;
    let config = supported.config();
    let (channels, rate) = (usize::from(config.channels), config.sample_rate);
    let playout: Arc<Mutex<Playout>> = Arc::default();
    let fill = (rate as f32 * PREBUFFER) as usize * CHANNELS;
    let fade = ((rate as f32 * FADE) as usize).max(1);
    let (drained, flag) = (playout.clone(), broken.clone());
    let stream = device
        .build_output_stream(
            config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let mut playout = drained.lock().unwrap();
                for frame in data.chunks_exact_mut(channels) {
                    let (left, right) = playout.next(fill, fade);
                    frame.fill(0.);
                    if channels > 1 {
                        frame[0] = left;
                        frame[1] = right;
                    } else {
                        frame[0] = (left + right) / 2.;
                    }
                }
            },
            move |err| {
                eprintln!("mediad: speaker: {err}");
                flag.store(true, Ordering::Relaxed);
            },
            None,
        )
        .map_err(|e| AudioError::Speaker(e.to_string()))?;
    stream
        .play()
        .map_err(|e| AudioError::Speaker(e.to_string()))?;
    let room = (rate as f32 * BACKLOG) as usize * CHANNELS;
    Ok(Output {
        _stream: stream,
        playout,
        resampler: Resampler::new(RATE, rate),
        room,
        fill,
        fade,
    })
}

impl Audio {
    /// True when the environment replaced the devices, so no permission applies.
    pub fn simulated() -> bool {
        std::env::var_os("TINYCLIENT_MEDIAD_AUDIO").is_some()
    }

    /// Opens the microphone and speaker, or their stand-ins.
    pub fn open(
        want_input: Option<String>,
        want_output: Option<String>,
    ) -> Result<Audio, AudioError> {
        Audio::open_sides(want_input, want_output, (true, true))
    }

    /// Opens only the microphone (`sides.0`) or only the speaker (`sides.1`), for a test.
    pub fn open_sides(
        want_input: Option<String>,
        want_output: Option<String>,
        sides: (bool, bool),
    ) -> Result<Audio, AudioError> {
        let mut audio = Audio {
            source: Source::Device(None),
            sink: Sink::Device(None),
            captured: Vec::new(),
            scratch: Vec::new(),
            broken: Arc::default(),
            want_input,
            want_output,
            input: String::new(),
            output: String::new(),
            sides,
        };
        if let Ok(kind) = std::env::var("TINYCLIENT_MEDIAD_AUDIO") {
            audio.source = match kind.as_str() {
                "tone" => Source::Tone { phase: 0. },
                "onset" => Source::Onset { at: 0 },
                _ => Source::Silence,
            };
            audio.sink = match std::env::var_os("TINYCLIENT_MEDIAD_SINK") {
                Some(path) => {
                    Sink::File(File::create(path).map_err(|e| AudioError::Speaker(e.to_string()))?)
                }
                None => Sink::Null,
            };
            audio.input = kind;
            audio.output = "null".to_owned();
            return Ok(audio);
        }
        audio.reopen()?;
        Ok(audio)
    }

    /// Real devices, not the stand-ins a test asks for.
    pub(crate) fn on_devices(&self) -> bool {
        matches!(self.source, Source::Device(_))
    }

    pub(crate) fn mic_running(&self) -> bool {
        match &self.source {
            Source::Device(Some(input)) => input.running,
            _ => true,
        }
    }

    pub(crate) fn device_broken(&self) -> bool {
        self.broken.load(Ordering::Relaxed)
    }

    pub(crate) fn sides(&self) -> (bool, bool) {
        self.sides
    }

    /// Opens the wanted devices again: at start, before the 20 ms loop is running.
    pub fn reopen(&mut self) -> Result<(), AudioError> {
        if !self.on_devices() {
            return Ok(());
        }
        let fresh = open_fresh(
            self.want_input.as_deref(),
            self.want_output.as_deref(),
            self.sides,
            self.mic_running(),
        )?;
        drop(self.install(fresh));
        Ok(())
    }

    /// Takes devices that were opened off the call thread. The returned streams are the ones
    /// just replaced; drop them off the 20 ms thread.
    pub(crate) fn install(&mut self, fresh: Fresh) -> Retired {
        let retired = Retired {
            source: std::mem::replace(&mut self.source, fresh.source),
            sink: std::mem::replace(&mut self.sink, fresh.sink),
        };
        self.broken = fresh.broken;
        self.input = fresh.input;
        self.output = fresh.output;
        self.captured.clear();
        self.scratch.clear();
        retired
    }

    /// True when a device stopped, or the system default we follow moved to another device.
    pub fn moved(&self) -> bool {
        if !self.on_devices() {
            return false;
        }
        self.device_broken()
            || route_moved(
                &self.want_input,
                &self.input,
                &self.want_output,
                &self.output,
                self.sides,
            )
    }

    /// Stops or starts the microphone itself, so a muted call shows no recording indicator.
    pub fn listen(&mut self, on: bool) {
        if let Source::Device(Some(input)) = &mut self.source {
            if input.running != on {
                let changed = if on {
                    input.stream.play()
                } else {
                    input.stream.pause()
                };
                if let Err(err) = changed {
                    eprintln!("mediad: microphone: {err}");
                }
                input.running = on;
                input.ring.lock().unwrap().clear();
                self.captured.clear();
            }
        }
    }

    /// The next 20 ms from the microphone, if that much has arrived. Called once a tick.
    pub fn capture(&mut self, frame: &mut [i16; FRAME_SAMPLES]) -> bool {
        match &mut self.source {
            Source::Silence => {
                frame.fill(0);
                true
            }
            Source::Tone { phase } => {
                for pair in frame.as_chunks_mut::<CHANNELS>().0 {
                    pair.fill((phase.sin() * 8000.) as i16);
                    *phase = (*phase + 440. * std::f64::consts::TAU / f64::from(RATE))
                        % std::f64::consts::TAU;
                }
                true
            }
            Source::Onset { at } => {
                for pair in frame.as_chunks_mut::<CHANNELS>().0 {
                    let peak = match *at % RATE {
                        0..24_000 => 0.,
                        24_000..25_920 => 100.,
                        _ => 8000.,
                    };
                    let angle = f64::from(*at) * 440. * std::f64::consts::TAU / f64::from(RATE);
                    pair.fill((angle.sin() * peak) as i16);
                    *at = at.wrapping_add(1);
                }
                true
            }
            Source::Device(None) => false,
            Source::Device(Some(input)) => {
                if !input.running {
                    return false;
                }
                self.scratch.clear();
                self.scratch.extend(input.ring.lock().unwrap().drain(..));
                input.resampler.push(&self.scratch, &mut self.captured);
                // Keep at most a few frames: what is older is delay nobody wants.
                let over = self.captured.len().saturating_sub(FRAME_SAMPLES * 5);
                self.captured.drain(..over - over % FRAME_SAMPLES);
                if self.captured.len() < FRAME_SAMPLES {
                    return false;
                }
                for (out, sample) in frame.iter_mut().zip(self.captured.drain(..FRAME_SAMPLES)) {
                    *out = (sample.clamp(-1., 1.) * 32767.) as i16;
                }
                true
            }
        }
    }

    /// 20 ms of the mix, to the speaker.
    pub fn play(&mut self, frame: &[i16; FRAME_SAMPLES]) {
        match &mut self.sink {
            Sink::Null | Sink::Device(None) => {}
            Sink::File(file) => {
                let bytes: Vec<u8> = frame.iter().flat_map(|s| s.to_le_bytes()).collect();
                let _ = file.write_all(&bytes);
            }
            Sink::Device(Some(output)) => {
                self.scratch.clear();
                self.scratch
                    .extend(frame.iter().map(|&s| f32::from(s) / 32768.));
                let mut resampled = Vec::with_capacity(FRAME_SAMPLES * 2);
                output.resampler.push(&self.scratch, &mut resampled);
                let mut playout = output.playout.lock().unwrap();
                playout.ring.extend(resampled);
                if playout.ring.len() > output.room {
                    // Our clock runs ahead of the device's. Skip to the cushion: one short
                    // ramp now rather than a delay that grows.
                    playout.trim_front(output.fill, output.fade);
                }
            }
        }
    }
}

/// The speaker alone, for playing a file (`mediad --play`): no microphone is opened, so no
/// permission is asked and no recording indicator shows. Unlike a call it never drops what it
/// is given; the player waits on [`Speaker::queued`] instead.
pub struct Speaker {
    sink: Sink,
    /// For a stand-in sink, which has no device to drain it: when it began draining and how
    /// much it was given since, so it drains at the pace a device would.
    began: Instant,
    given: u64,
    scratch: Vec<f32>,
    broken: Arc<AtomicBool>,
    pub name: String,
}

impl Speaker {
    /// The device named `want`, else the system default. `TINYCLIENT_MEDIAD_AUDIO` replaces it
    /// with nothing, or with `TINYCLIENT_MEDIAD_SINK`'s file, as for a call.
    pub fn open(want: Option<&str>) -> Result<Speaker, AudioError> {
        let mut speaker = Speaker {
            sink: Sink::Null,
            began: Instant::now(),
            given: 0,
            scratch: Vec::new(),
            broken: Arc::default(),
            name: "null".to_owned(),
        };
        if Audio::simulated() {
            if let Some(path) = std::env::var_os("TINYCLIENT_MEDIAD_SINK") {
                speaker.sink =
                    Sink::File(File::create(path).map_err(|e| AudioError::Speaker(e.to_string()))?);
            }
            return Ok(speaker);
        }
        let host = cpal::default_host();
        let device = find(&host, want, false).ok_or(AudioError::NoSpeaker)?;
        speaker.sink = Sink::Device(Some(open_output(&device, &speaker.broken)?));
        speaker.name = name(&device).unwrap_or_default();
        Ok(speaker)
    }

    /// 48 kHz frames given and not yet played.
    pub fn queued(&self) -> usize {
        match &self.sink {
            Sink::Device(Some(output)) => {
                let held = output.playout.lock().unwrap().ring.len() / CHANNELS;
                // The ring is at the device's rate; the player counts at ours.
                (held as f64 * output.resampler.step) as usize
            }
            _ => {
                let drained = (self.began.elapsed().as_secs_f64() * f64::from(RATE)) as u64;
                self.given.saturating_sub(drained) as usize
            }
        }
    }

    /// Interleaved 48 kHz stereo, played after what is already queued.
    pub fn push(&mut self, pcm: &[f32]) {
        if self.queued() == 0 {
            // A stand-in starts draining when it is given something, as a device would.
            self.began = Instant::now();
            self.given = 0;
        }
        self.given += (pcm.len() / CHANNELS) as u64;
        match &mut self.sink {
            Sink::Null | Sink::Device(None) => {}
            Sink::File(file) => {
                let bytes: Vec<u8> = pcm
                    .iter()
                    .flat_map(|s| ((s.clamp(-1., 1.) * 32767.) as i16).to_le_bytes())
                    .collect();
                let _ = file.write_all(&bytes);
            }
            Sink::Device(Some(output)) => {
                self.scratch.clear();
                output.resampler.push(pcm, &mut self.scratch);
                let mut playout = output.playout.lock().unwrap();
                playout.ring.extend(self.scratch.iter());
                playout.flush = false;
            }
        }
    }

    /// Nothing more is coming: what is held plays even if it is shorter than the cushion.
    pub fn finish(&mut self) {
        if let Sink::Device(Some(output)) = &mut self.sink {
            output.playout.lock().unwrap().flush = true;
        }
    }

    /// Drops what is queued, so a seek or a new speed is heard at once.
    pub fn clear(&mut self) {
        if let Sink::Device(Some(output)) = &mut self.sink {
            let mut playout = output.playout.lock().unwrap();
            playout.ring.clear();
            playout.playing = false;
        }
        self.given = 0;
    }

    /// The device stopped or went away.
    pub fn broken(&self) -> bool {
        self.broken.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, hertz: f32, frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|n| [(n as f32 * hertz * std::f32::consts::TAU / rate as f32).sin(); 2])
            .collect()
    }

    /// Resamples in uneven pieces, as a device delivers them.
    fn resampled(from: u32, to: u32, input: &[f32]) -> Vec<f32> {
        let mut resampler = Resampler::new(from, to);
        let mut output = Vec::new();
        for piece in input.chunks(2 * 441 + 2) {
            resampler.push(piece, &mut output);
        }
        output
    }

    /// The largest distance between `output` and a tone of `hertz` at `rate`, past the start.
    fn worst(output: &[f32], rate: u32, hertz: f32) -> f32 {
        let frames = output.len() / 2;
        (200..frames)
            .map(|n| {
                (output[2 * n] - (n as f32 * hertz * std::f32::consts::TAU / rate as f32).sin())
                    .abs()
            })
            .fold(0f32, f32::max)
    }

    #[test]
    fn resampling_keeps_the_duration_and_the_shape() {
        // 100 ms of a 440 Hz tone at 44.1 kHz becomes 100 ms at 48 kHz, less the filter's reach.
        let output = resampled(44_100, 48_000, &tone(44_100, 440., 4410));
        let frames = output.len() / 2;
        assert!((4775..=4800).contains(&frames), "{frames} frames");
        // Each output sample is the tone at its own time: no delay to account for.
        let off = worst(&output, 48_000, 440.);
        assert!(off < 0.002, "off by {off}");
    }

    #[test]
    fn resampling_keeps_high_frequencies_that_a_straight_line_would_bend() {
        // 9 kHz is where consonants live; linear interpolation is off by a quarter there.
        for (from, to) in [
            (44_100, 48_000),
            (48_000, 44_100),
            (16_000, 48_000),
            (96_000, 48_000),
        ] {
            let hertz = if from == 16_000 { 5_000. } else { 9_000. };
            let output = resampled(from, to, &tone(from, hertz, from as usize / 10));
            let off = worst(&output, to, hertz);
            assert!(off < 0.01, "{from} to {to}: off by {off}");
        }
    }

    #[test]
    fn going_down_removes_what_the_new_rate_cannot_carry() {
        // 30 kHz at 96 kHz would fold to 18 kHz at 48 kHz. It has to be gone instead.
        let output = resampled(96_000, 48_000, &tone(96_000, 30_000., 9_600));
        let loudest = output[400..].iter().fold(0f32, |max, s| max.max(s.abs()));
        assert!(loudest < 0.003, "an alias at {loudest}");
    }

    #[test]
    fn a_steady_level_stays_steady_at_every_phase() {
        let output = resampled(44_100, 48_000, &vec![0.5; 2 * 4410]);
        let (low, high) = output[200..]
            .iter()
            .fold((1f32, 0f32), |(low, high), &s| (low.min(s), high.max(s)));
        assert!(low > 0.4999 && high < 0.5001, "{low} to {high}");
    }

    #[test]
    fn the_speaker_is_given_nothing_until_the_cushion_is_held_and_refills_after_running_dry() {
        let (fill, fade) = (8, 2);
        let mut playout = Playout::default();
        playout.ring.extend([0.5; 6]);
        assert_eq!(
            playout.next(fill, fade),
            (0., 0.),
            "three frames held, four wanted"
        );
        assert_eq!(playout.ring.len(), 6, "and nothing was taken");
        playout.ring.extend([0.5; 2]);
        // Playing starts faded in, so even a loud first sample does not click.
        assert_eq!(playout.next(fill, fade), (0., 0.));
        assert_eq!(playout.next(fill, fade), (0.25, 0.25));
        // The last two frames ramp down, so the underrun that follows is already silence.
        assert_eq!(playout.next(fill, fade), (0.25, 0.25));
        assert_eq!(playout.next(fill, fade), (0., 0.));
        // Dry: silence, and the cushion is built again before anything plays.
        assert_eq!(playout.next(fill, fade), (0., 0.));
        assert_eq!(playout.underruns, 1);
        playout.ring.extend([0.5; 4]);
        assert_eq!(playout.next(fill, fade), (0., 0.));
        assert_eq!(playout.ring.len(), 4);
    }

    /// The biggest jump between one played sample and the next.
    fn steepest(played: &[(f32, f32)]) -> f32 {
        played.windows(2).fold(0f32, |steepest, pair| {
            let step = (pair[1].0 - pair[0].0)
                .abs()
                .max((pair[1].1 - pair[0].1).abs());
            steepest.max(step)
        })
    }

    #[test]
    fn running_dry_ramps_to_silence_instead_of_stepping() {
        let (fill, fade) = (4, 8);
        let mut playout = Playout::default();
        playout.ring.extend([1.; 40]);
        let mut played = Vec::new();
        for _ in 0..24 {
            played.push(playout.next(fill, fade));
        }
        assert_eq!(playout.underruns, 1);
        assert!(played.last() == Some(&(0., 0.)));
        let step = steepest(&played);
        assert!(step <= 1. / fade as f32 + 1e-4, "a step of {step}");
    }

    #[test]
    fn skipping_ahead_crossfades_the_splice() {
        let mut playout = Playout::default();
        playout.ring.extend([0.; 20]);
        playout.ring.extend([1.; 8]);
        playout.trim_front(8, 4);
        assert_eq!(playout.ring.len(), 8);
        let samples: Vec<f32> = playout.ring.iter().copied().collect();
        let played: Vec<(f32, f32)> = samples
            .chunks(2)
            .map(|frame| (frame[0], frame[1]))
            .collect();
        let step = steepest(&played);
        assert!(step < 0.25, "a splice of {step}: {played:?}");
        assert!(played[0].0 < 0.3 && played[3].0 > 0.7, "{played:?}");
    }

    #[test]
    fn level_is_zero_for_silence_and_rises_with_loudness() {
        assert_eq!(level(&[0; 1920]), 0);
        let quiet = level(&[33; 1920]);
        let speech = level(&[3000; 1920]);
        let full = level(&[32767; 1920]);
        assert!(quiet < 5, "{quiet}");
        assert!((55..75).contains(&speech), "{speech}");
        assert_eq!(full, 99);
    }
}
