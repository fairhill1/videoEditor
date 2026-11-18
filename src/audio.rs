use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ffmpeg_next as ffmpeg;

use crate::media::MediaPool;
use std::collections::HashMap;

use crate::timeline::{Clip, SourceId};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;

// How much audio to keep queued ahead of the output callback. Larger absorbs
// render-loop stalls; smaller tightens scrub/seek latency. 150ms is a sane
// default — below typical frame-stutter thresholds, above any single render hit.
const TARGET_BUFFER_SEC: f64 = 0.15;
// Mix granularity. Clip boundaries inside one tick only re-query the timeline
// at chunk boundaries — small enough to land hard cuts within ~20ms.
const MIX_CHUNK_FRAMES: usize = 1024;
const AV_TIME_BASE: f64 = 1_000_000.0;
// If an audio stream's read position differs from where we want to sample next
// by more than this, issue a seek. Picked larger than any natural
// chunk-to-chunk advance (~21ms) so contiguous playback doesn't thrash seeks,
// and smaller than perceivable drift.
const RESEEK_THRESHOLD_SEC: f64 = 0.030;
// How far before its target a seek starts decoding. Comfortably more than the
// one or two frames a decoder needs to warm up after a jump.
const SEEK_PREROLL_SEC: f64 = 0.2;
// How long a scrub grain lasts: the sound a drag of the playhead plays at
// each place it passes. Long enough to tell a word or a beat from a hiss,
// short enough that a quick drag still sounds like it is moving.
const GRAIN_SEC: f64 = 0.08;
// The fade at each end of a grain. A grain is cut out of the middle of the
// sound, and starting or stopping it on a hard edge clicks.
const GRAIN_FADE_SEC: f64 = 0.004;

// Peaks per second baked into the waveform summary. 50 Hz ≈ one peak every
// 20ms, fine-grained enough that transients are visible at any practical zoom
// without blowing memory for long sources.
const WAVEFORM_PEAKS_PER_SEC: f64 = 50.0;

/// Pre-computed absolute-peak summary of a source's audio. Built once on import;
/// the UI samples this to draw the waveform under audio clips.
pub struct Waveform {
    pub peaks: Vec<f32>,
    /// Duration of source audio each `peaks[i]` summarizes.
    pub bucket_seconds: f64,
}

/// Give a decoded frame the channel layout its decoder implies, when the codec
/// did not state one.
///
/// PCM decoders leave a frame's layout unset, and `swr_convert_frame` compares
/// the frame's own layout against the one its context was built with — so
/// without this a WAV resamples to nothing and imports as silence, while an AAC
/// track (which always carries a layout) works. It is the difference between
/// importing a music bed and importing five seconds of nothing.
fn ensure_frame_layout(frame: &mut ffmpeg::frame::Audio, layout: ffmpeg::ChannelLayout) {
    if frame.channel_layout().bits() == 0 {
        frame.set_channel_layout(layout);
    }
}

/// The layout a decoder means when it reports none: the default arrangement for
/// however many channels it has.
fn decoder_layout(decoder: &ffmpeg::decoder::Audio) -> ffmpeg::ChannelLayout {
    let l = decoder.channel_layout();
    if l.bits() != 0 {
        l
    } else {
        ffmpeg::ChannelLayout::default(decoder.channels() as i32)
    }
}

/// Next stereo frame from the mix buffer, or silence once it has run dry.
///
/// A short read is an underrun, not an error: the render thread refills between
/// frames, and a stall there should cost a gap rather than a stopped stream.
fn pop_frame(buffer: &mut VecDeque<f32>) -> [f32; CHANNELS] {
    let l = buffer.pop_front().unwrap_or(0.0);
    let r = buffer.pop_front().unwrap_or(l);
    [l, r]
}

/// Streaming audio decoder for one source file. Mirrors `VideoStream`: opens its
/// own ffmpeg `Input` so seeks are independent of the video side, resamples into
/// interleaved stereo f32 at `SAMPLE_RATE`, and keeps a small pending buffer so
/// callers can pull arbitrary counts.
pub struct AudioStream {
    ictx: ffmpeg::format::context::Input,
    decoder: ffmpeg::decoder::Audio,
    resampler: ffmpeg::software::resampling::Context,
    /// What the decoder's frames are laid out as, for [`ensure_frame_layout`].
    in_layout: ffmpeg::ChannelLayout,
    stream_index: usize,
    time_base_seconds: f64,
    /// Where the stream's timestamps start: an MP3's encoder delay, say. The
    /// first sample decoded is time 0 here, so timestamps have this taken off.
    start_seconds: f64,
    duration_seconds: f64,
    pending: VecDeque<f32>,
    /// The packets have run out and the decoder has been told so: what it
    /// still holds is being drained.
    draining: bool,
    /// Timeline time of the next sample to be returned by `read`.
    position: f64,
    eof: bool,
    /// The file's own rate and channel count, kept for the media pool to show.
    /// Everything downstream of the resampler is [`SAMPLE_RATE`] stereo, so
    /// these are the only place the source's real format survives.
    src_rate: u32,
    src_channels: u16,
}

impl AudioStream {
    /// Returns `Ok(None)` for files with no audio stream; `Err` only for real
    /// decode/setup failures. Callers treat `None` as "video-only source".
    pub fn open(path: &str) -> Result<Option<Self>, ffmpeg::Error> {
        let ictx = ffmpeg::format::input(&path)?;

        let duration_raw = ictx.duration();
        let duration_seconds = if duration_raw > 0 {
            duration_raw as f64 / AV_TIME_BASE
        } else {
            0.0
        };

        let (stream_index, time_base_seconds, start_seconds, parameters) = {
            let Some(input) = ictx.streams().best(ffmpeg::media::Type::Audio) else {
                return Ok(None);
            };
            let tb = input.time_base();
            let tb = tb.numerator() as f64 / tb.denominator() as f64;
            // Unknown comes back as the most negative timestamp there is.
            let start = input.start_time();
            let start_seconds = if start > 0 { start as f64 * tb } else { 0.0 };
            (input.index(), tb, start_seconds, input.parameters())
        };

        let ctx = ffmpeg::codec::context::Context::from_parameters(parameters)?;
        let decoder = ctx.decoder().audio()?;

        let in_layout = decoder_layout(&decoder);

        let resampler = new_resampler(&decoder, in_layout)?;

        Ok(Some(Self {
            src_rate: decoder.rate(),
            src_channels: decoder.channels(),
            ictx,
            decoder,
            resampler,
            in_layout,
            stream_index,
            time_base_seconds,
            start_seconds,
            duration_seconds,
            pending: VecDeque::new(),
            draining: false,
            position: 0.0,
            eof: false,
        }))
    }

    pub fn duration(&self) -> f64 {
        self.duration_seconds
    }

    pub fn src_rate(&self) -> u32 {
        self.src_rate
    }

    pub fn src_channels(&self) -> u16 {
        self.src_channels
    }

    pub fn position(&self) -> f64 {
        self.position
    }

    /// Seek so that subsequent `read` calls begin at `t`. Demuxer lands on the
    /// nearest keyframe before `t`; we drop decoded samples from the head to
    /// bring the stream exactly to `t`.
    pub fn seek(&mut self, t: f64) {
        let t = t.max(0.0);
        // A little before `t`, and decoded up to it: the first frame after
        // a seek can come out silent — an MP3 frame borrows bits from the one
        // before it — and this way that frame is one of those dropped.
        let from = (t - SEEK_PREROLL_SEC).max(0.0) + self.start_seconds;
        let ts = (from * AV_TIME_BASE) as i64;
        let _ = self.ictx.seek(ts, ..);
        self.decoder.flush();
        // A fresh resampler too: the old one holds the tail of wherever the
        // stream was before, which would otherwise play first.
        if let Ok(resampler) = new_resampler(&self.decoder, self.in_layout) {
            self.resampler = resampler;
        }
        self.pending.clear();
        self.position = t;
        self.eof = false;
        self.draining = false;

        // Prime one frame so we know where the demuxer actually landed, and
        // drop samples to align to `t`.
        let Some(first_pts) = self.decode_into_pending() else {
            return;
        };
        // The stream's first frame reads as before 0 once the start is taken
        // off, but the decoder has already trimmed the delay from it: what it
        // gives starts at 0.
        let first_pts = first_pts.max(0.0);
        let lead = t - first_pts;
        if lead > 0.0 {
            let mut to_drop_samples = (lead * SAMPLE_RATE as f64).round() as usize * CHANNELS;
            while to_drop_samples > 0 {
                if self.pending.is_empty() && self.decode_into_pending().is_none() {
                    break;
                }
                let take = to_drop_samples.min(self.pending.len());
                for _ in 0..take {
                    self.pending.pop_front();
                }
                to_drop_samples -= take;
            }
        } else if lead < 0.0 {
            // Demuxer sometimes lands after `t` (e.g. near duration). Let
            // caller know we're slightly ahead so it won't loop re-seeking.
            self.position = first_pts;
        }
    }

    /// Fill `out` with up to `out.len()` samples from the current position.
    /// Returns the number of samples actually written; short reads mean EOF.
    pub fn read(&mut self, out: &mut [f32]) -> usize {
        let mut written = 0;
        while written < out.len() {
            if self.pending.is_empty() && self.decode_into_pending().is_none() {
                break;
            }
            let take = (out.len() - written).min(self.pending.len());
            for i in 0..take {
                out[written + i] = self.pending.pop_front().unwrap();
            }
            written += take;
        }
        self.position += (written / CHANNELS) as f64 / SAMPLE_RATE as f64;
        written
    }

    /// Decode+resample one frame's worth of audio into `pending`. Returns the
    /// pts (seconds) of the decoded source frame, or `None` on EOF.
    fn decode_into_pending(&mut self) -> Option<f64> {
        if self.eof {
            return None;
        }
        let mut frame = ffmpeg::frame::Audio::empty();
        loop {
            match self.decoder.receive_frame(&mut frame) {
                Ok(()) => {
                    ensure_frame_layout(&mut frame, self.in_layout);
                    let pts = frame.pts().unwrap_or(0);
                    let pts_sec = pts as f64 * self.time_base_seconds - self.start_seconds;
                    let mut resampled = self.output_frame(frame.samples());
                    if self.resampler.run(&frame, &mut resampled).is_ok() {
                        self.take_resampled(&resampled);
                    }
                    return Some(pts_sec);
                }
                Err(_) if self.draining => {
                    // The decoder is empty: what the resampler still holds
                    // is the very end of the file.
                    self.eof = true;
                    let mut tail = self.output_frame(0);
                    if self.resampler.flush(&mut tail).is_ok() && tail.samples() > 0 {
                        self.take_resampled(&tail);
                        return Some(self.duration_seconds);
                    }
                    return None;
                }
                Err(_) => match self.next_audio_packet() {
                    Some(packet) => {
                        let _ = self.decoder.send_packet(&packet);
                    }
                    None => {
                        let _ = self.decoder.send_eof();
                        self.draining = true;
                    }
                },
            }
        }
    }

    /// An empty output frame with room for everything `in_samples` input
    /// samples can come out as, plus what the resampler is holding back.
    ///
    /// Sized here rather than left to `run`, which makes it as many samples
    /// as went in: too few whenever the rate goes up, as 44.1kHz to 48kHz
    /// does. The rest then piled up inside the resampler, falling further
    /// behind with every frame — 0.67s by the end of an 8s MP3 — and played
    /// out after a seek as the audio from before it.
    fn output_frame(&self, in_samples: usize) -> ffmpeg::frame::Audio {
        let rate = self.src_rate.max(1) as u64;
        let held = self.resampler.delay().map_or(0, |d| d.output.max(0) as u64);
        let room = (in_samples as u64 * SAMPLE_RATE as u64).div_ceil(rate) + held + 64;
        ffmpeg::frame::Audio::new(
            ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Packed),
            room as usize,
            ffmpeg::ChannelLayout::STEREO,
        )
    }

    fn take_resampled(&mut self, resampled: &ffmpeg::frame::Audio) {
        let total_bytes = resampled.samples() * CHANNELS * 4;
        let bytes = resampled.data(0);
        if bytes.len() >= total_bytes {
            for chunk in bytes[..total_bytes].chunks_exact(4) {
                self.pending.push_back(f32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
            }
        }
    }

    fn next_audio_packet(&mut self) -> Option<ffmpeg::Packet> {
        let mut iter = self.ictx.packets();
        loop {
            let (stream, packet) = iter.next()?;
            if stream.index() == self.stream_index {
                return Some(packet);
            }
        }
    }
}

/// The converter from `decoder`'s format to the mix's: interleaved stereo
/// f32 at [`SAMPLE_RATE`].
fn new_resampler(
    decoder: &ffmpeg::decoder::Audio,
    in_layout: ffmpeg::ChannelLayout,
) -> Result<ffmpeg::software::resampling::Context, ffmpeg::Error> {
    ffmpeg::software::resampling::Context::get(
        decoder.format(),
        in_layout,
        decoder.rate(),
        ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Packed),
        ffmpeg::ChannelLayout::STEREO,
        SAMPLE_RATE,
    )
}

/// Readers for clips that play at the same moment as another clip of the
/// same file.
///
/// A reader holds one place in its file. Two clips of one file playing at
/// once — a sound layered over itself, the halves of a split moved to
/// overlap — sharing one reader would drag it back and forth between their
/// two places every chunk, which sounds like the audio chopped up and run
/// backwards. So the first such clip in track order reads with the file's
/// own reader, and each other gets one of its own from here, kept while it
/// plays.
#[derive(Default)]
pub struct Voices {
    open: HashMap<(usize, u32), (AudioStream, u64)>,
    round: u64,
}

impl Voices {
    /// Start a mixing pass. Readers not used for a couple of passes are
    /// closed at the start of the next.
    pub fn begin_round(&mut self) {
        self.round += 1;
        let round = self.round;
        self.open.retain(|_, (_, used)| *used + 2 >= round);
    }

    /// The reader for clip `clip` on track `track`, opening `path` for it the
    /// first time.
    pub fn get(&mut self, track: usize, clip: u32, path: &str) -> Option<&mut AudioStream> {
        let round = self.round;
        let entry = match self.open.entry((track, clip)) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let stream = match AudioStream::open(path) {
                    Ok(Some(stream)) => stream,
                    Ok(None) => return None,
                    Err(err) => {
                        log::warn!("could not open a second reader for {path}: {err}");
                        return None;
                    }
                };
                e.insert((stream, round))
            }
        };
        entry.1 = round;
        Some(&mut entry.0)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.open.len()
    }
}

/// Add `clip`'s sound at timeline time `t` into `mix`, read from `stream`:
/// moved to the right place in the file first if it is not already there,
/// then scaled by the clip's level sample by sample. Both mixers' one way of
/// playing a clip.
pub fn mix_clip(
    stream: &mut AudioStream,
    clip: &Clip,
    t: f64,
    scratch: &mut [f32],
    mix: &mut [f32],
) {
    let src_t = clip.source_time(t);
    if (stream.position() - src_t).abs() > RESEEK_THRESHOLD_SEC {
        stream.seek(src_t);
    }
    // Zeroed first because a short read at EOF leaves the tail holding
    // whatever was there before.
    scratch.fill(0.0);
    let n = stream.read(scratch);
    // Per sample rather than per chunk: a fade is a ramp, and holding one
    // value for 21ms of it would step the level in audible stairs.
    let dt = 1.0 / SAMPLE_RATE as f64;
    for i in 0..n {
        mix[i] += scratch[i] * clip.level(t + (i / CHANNELS) as f64 * dt);
    }
}

// ---------------------------------------------------------------------------

struct Shared {
    /// Interleaved stereo f32 at `SAMPLE_RATE`.
    buffer: VecDeque<f32>,
    /// Timeline time of the next sample the cpal callback will consume.
    /// Advances in real time when `playing`. Not what is being heard: the
    /// device still has its own queue to play before it gets there. See
    /// [`heard`].
    consume_t: f64,
    playing: bool,
    /// The last block handed to the device while playing: the timeline time
    /// of its first sample, and when the device says that sample reaches the
    /// speaker. `None` until the first block after a start or a jump.
    anchor: Option<(f64, Instant)>,
    /// Where playback started, or was last moved to while playing. Nothing
    /// before it is heard, so the clock does not run back to before it while
    /// the device's queue is still playing out.
    play_from: f64,
    /// The device's queue as of its last block, in seconds: how far the
    /// picture is held back to meet the sound. Kept to be logged.
    delay: f64,
    /// While stopped: the buffer holds a scrub grain for the callback to
    /// play out, without moving the clock.
    grain: bool,
    /// Where the last grain was taken, so a playhead held still under the
    /// pointer goes quiet rather than repeating itself.
    grain_at: f64,
}

/// Owns the cpal output stream and the mixer. The cpal callback on the audio
/// thread drains `Shared::buffer` and advances `consume_t`; the render thread
/// calls `tick` once per frame to refill the buffer from the current timeline.
pub struct AudioEngine {
    _stream: Option<cpal::Stream>,
    shared: Arc<Mutex<Shared>>,
    voices: Mutex<Voices>,
    /// The device delay last logged, in whole milliseconds.
    logged_delay: Mutex<i64>,
}

impl AudioEngine {
    pub fn new() -> Self {
        let shared = Arc::new(Mutex::new(Shared {
            buffer: VecDeque::with_capacity(SAMPLE_RATE as usize * CHANNELS),
            consume_t: 0.0,
            playing: false,
            anchor: None,
            play_from: 0.0,
            delay: 0.0,
            grain: false,
            grain_at: f64::NAN,
        }));

        let stream = match Self::build_stream(&shared) {
            Ok(s) => Some(s),
            Err(e) => {
                log::error!("audio output init failed: {e}");
                None
            }
        };

        Self {
            _stream: stream,
            shared,
            voices: Mutex::new(Voices::default()),
            logged_delay: Mutex::new(0),
        }
    }

    /// Prefer a device configuration at our own mix rate.
    ///
    /// Everything upstream of the output — the mixer, the clip envelopes, the
    /// exported file — runs at [`SAMPLE_RATE`], so a device willing to take
    /// 48kHz needs no conversion at all. Many will even when their default
    /// config says otherwise, which is why this asks rather than trusting the
    /// default; the callback only resamples when nothing on offer fits.
    fn pick_output_config(device: &cpal::Device) -> Result<cpal::SupportedStreamConfig, String> {
        let default = device.default_output_config().map_err(|e| e.to_string())?;
        if default.sample_rate() == SAMPLE_RATE {
            return Ok(default);
        }
        let matched = device
            .supported_output_configs()
            .map_err(|e| e.to_string())?
            .find(|r| {
                r.sample_format() == default.sample_format()
                    && r.channels() == default.channels()
                    && r.min_sample_rate() <= SAMPLE_RATE
                    && r.max_sample_rate() >= SAMPLE_RATE
            })
            .map(|r| r.with_sample_rate(SAMPLE_RATE));
        Ok(matched.unwrap_or(default))
    }

    fn build_stream(shared: &Arc<Mutex<Shared>>) -> Result<cpal::Stream, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "no default output device".to_string())?;
        let supported = Self::pick_output_config(&device)?;

        let device_channels = supported.channels() as usize;
        let device_rate = supported.sample_rate();

        if supported.sample_format() != cpal::SampleFormat::F32 {
            return Err(format!(
                "unsupported device sample format: {:?}",
                supported.sample_format()
            ));
        }

        // Mix frames consumed per output frame. Exactly 1.0 whenever the device
        // took our rate, and the interpolation below is then a straight copy —
        // so the common case pays nothing for the uncommon one being handled.
        let step = SAMPLE_RATE as f64 / device_rate as f64;
        if device_rate != SAMPLE_RATE {
            log::info!(
                "audio device runs at {device_rate} Hz; resampling the {SAMPLE_RATE} Hz mix into it"
            );
        }

        let config: cpal::StreamConfig = supported.into();
        let shared_cb = shared.clone();
        // Resampler state, carried across callbacks: the two mix frames the next
        // output frame falls between, and how far between them it is. Owned by
        // the closure rather than by `Shared` because nothing else reads it —
        // and a seek, which drops the buffer, is welcome to leave it stale: the
        // cost is one frame of interpolation across the seam.
        let mut cur = [0.0f32; CHANNELS];
        let mut nxt = [0.0f32; CHANNELS];
        let mut frac = 0.0f64;
        let mut primed = false;
        let stream = device
            .build_output_stream(
                config,
                move |out: &mut [f32], info: &cpal::OutputCallbackInfo| {
                    let mut shared = shared_cb.lock().unwrap();
                    let grain = !shared.playing && shared.grain;
                    if !shared.playing && !grain {
                        out.fill(0.0);
                        return;
                    }
                    if !grain {
                        // This block is heard once the device has played what
                        // it already holds; the picture waits for it.
                        let stamp = info.timestamp();
                        let delay = stamp.playback.duration_since(stamp.callback);
                        shared.anchor = Some((shared.consume_t, Instant::now() + delay));
                        shared.delay = delay.as_secs_f64();
                    }
                    for frame_slot in out.chunks_mut(device_channels) {
                        if !primed {
                            cur = pop_frame(&mut shared.buffer);
                            nxt = pop_frame(&mut shared.buffer);
                            primed = true;
                        }
                        let t = frac as f32;
                        let l = cur[0] + (nxt[0] - cur[0]) * t;
                        let r = cur[1] + (nxt[1] - cur[1]) * t;
                        frac += step;
                        while frac >= 1.0 {
                            cur = nxt;
                            nxt = pop_frame(&mut shared.buffer);
                            frac -= 1.0;
                        }
                        if !frame_slot.is_empty() {
                            frame_slot[0] = l;
                        }
                        if frame_slot.len() >= 2 {
                            frame_slot[1] = r;
                        }
                        for extra in frame_slot.iter_mut().skip(2) {
                            *extra = 0.0;
                        }
                    }
                    // A grain plays where the playhead stands; it does not move it.
                    if grain {
                        if shared.buffer.is_empty() {
                            shared.grain = false;
                        }
                        return;
                    }
                    // Advance by the whole block duration — if we underran and
                    // wrote silence, time still marched on. Counted in device
                    // seconds, which is what the listener hears however many
                    // mix frames went into producing them.
                    let frames = out.len() / device_channels.max(1);
                    shared.consume_t += frames as f64 / device_rate as f64;
                },
                |err| log::error!("audio stream error: {err}"),
                None,
            )
            .map_err(|e| e.to_string())?;

        stream.play().map_err(|e| e.to_string())?;
        Ok(stream)
    }

    /// The playhead: while playing, the timeline time being heard right now,
    /// which is what the picture has to show to stay in sync with it.
    pub fn position(&self) -> f64 {
        heard(&self.shared.lock().unwrap(), Instant::now())
    }

    /// Jump the playhead. Drops any pre-mixed samples so the next `tick` refills
    /// from the new time — scrub/seek should feel immediate, not drift through
    /// 150ms of stale buffer.
    ///
    /// A scrub grain still sounding is left to finish: a drag moves the
    /// playhead every frame, and cutting each grain off there would leave
    /// nothing but clicks.
    pub fn set_position(&self, t: f64) {
        let mut shared = self.shared.lock().unwrap();
        shared.consume_t = t.max(0.0);
        shared.play_from = shared.consume_t;
        shared.anchor = None;
        if !shared.grain {
            shared.buffer.clear();
        }
    }

    pub fn playing(&self) -> bool {
        self.shared.lock().unwrap().playing
    }

    pub fn set_playing(&self, playing: bool) {
        let mut shared = self.shared.lock().unwrap();
        Self::start_or_stop(&mut shared, playing);
    }

    pub fn toggle(&self) {
        let mut shared = self.shared.lock().unwrap();
        let playing = !shared.playing;
        Self::start_or_stop(&mut shared, playing);
    }

    /// Stopping drops what was mixed ahead. So does starting over the end of
    /// a scrub grain: the rest of the grain is from wherever the drag was,
    /// not from where playback begins.
    ///
    /// Stopping parks the clock on what was last heard, not on what was last
    /// handed to the device, or the playhead would jump on by the device's
    /// queue as playback stops.
    fn start_or_stop(shared: &mut Shared, playing: bool) {
        if shared.playing && !playing {
            shared.consume_t = heard(shared, Instant::now());
        }
        if playing && !shared.playing {
            shared.play_from = shared.consume_t;
        }
        shared.anchor = None;
        if !playing || shared.grain {
            shared.buffer.clear();
        }
        shared.grain = false;
        shared.playing = playing;
    }

    /// Sound a drag of the playhead: a short grain from where it now stands,
    /// once the last grain has finished and the playhead has moved since. One
    /// at a time and back to back, so a steady drag plays the timeline in
    /// pieces as the hand goes, and a held one goes quiet.
    pub fn scrub(&self, tracks: &[Vec<Clip>], media: &mut MediaPool) {
        let t = {
            let shared = self.shared.lock().unwrap();
            if shared.playing || shared.grain || shared.consume_t == shared.grain_at {
                return;
            }
            shared.consume_t
        };
        let frames = (GRAIN_SEC * SAMPLE_RATE as f64) as usize;
        let mut grain = vec![0.0f32; frames * CHANNELS];
        self.mix_at(t, tracks, media, &mut grain);
        shape_grain(&mut grain);
        let mut shared = self.shared.lock().unwrap();
        // Started playing, or moved again, while the grain was being mixed:
        // it is a grain from somewhere else now, and the next frame takes
        // another.
        if shared.playing || shared.consume_t != t {
            return;
        }
        shared.buffer.clear();
        shared.buffer.extend(grain);
        shared.grain = true;
        shared.grain_at = t;
    }

    /// Refill the output buffer up to `TARGET_BUFFER_SEC` ahead of the current
    /// consume point. Called from the render loop — cheap when there's already
    /// plenty buffered, does actual decode work when there isn't.
    /// `tracks` are the audio tracks to play: the timeline's own and those
    /// inside its compound clips, already on the timeline's clock — see
    /// [`crate::timeline::flatten_audio`].
    pub fn tick(&self, tracks: &[Vec<Clip>], media: &mut MediaPool) {
        let (fill_t, to_produce) = {
            let shared = self.shared.lock().unwrap();
            // The buffer is a grain's until it has played out.
            if shared.grain {
                return;
            }
            // From here rather than the audio thread, which must not block.
            let ms = (shared.delay * 1000.0).round() as i64;
            let mut logged = self.logged_delay.lock().unwrap();
            if (ms - *logged).abs() >= 10 {
                log::info!("audio output delay {ms} ms; the picture is held back to match");
                *logged = ms;
            }
            let target = (TARGET_BUFFER_SEC * SAMPLE_RATE as f64) as usize * CHANNELS;
            let have = shared.buffer.len();
            let need = target.saturating_sub(have);
            // Round down to whole frames.
            let need = (need / CHANNELS) * CHANNELS;
            let buffered_sec = (have / CHANNELS) as f64 / SAMPLE_RATE as f64;
            (shared.consume_t + buffered_sec, need)
        };
        if to_produce == 0 {
            return;
        }

        let mut mix = vec![0.0f32; to_produce];
        self.mix_at(fill_t, tracks, media, &mut mix);

        let mut shared = self.shared.lock().unwrap();
        // A grain taken while this was mixing owns the buffer now.
        if !shared.grain {
            shared.buffer.extend(mix.iter().copied());
        }
    }

    /// Mix every audio clip live from timeline time `fill_t` into `mix`,
    /// interleaved stereo, as long as `mix` is.
    fn mix_at(&self, mut fill_t: f64, tracks: &[Vec<Clip>], media: &mut MediaPool, mix: &mut [f32]) {
        let to_produce = mix.len();
        let mut scratch = vec![0.0f32; MIX_CHUNK_FRAMES * CHANNELS];
        let mut voices = self.voices.lock().unwrap();
        voices.begin_round();

        let mut cursor = 0;
        while cursor < to_produce {
            let chunk = (to_produce - cursor).min(MIX_CHUNK_FRAMES * CHANNELS);
            // Sum every audio clip that's live at `fill_t`. Multi-track mixing
            // is the editor-standard behavior — A1 + A2 both play.
            let mut claimed: Vec<SourceId> = Vec::new();
            for (track_idx, track) in tracks.iter().enumerate() {
                let Some(clip) = track.iter().find(|c| c.contains(fill_t)) else {
                    continue;
                };
                let clip = *clip;
                let Some(src) = media.get_mut(clip.source) else {
                    continue;
                };
                // The file's own reader for the first clip of it, a reader
                // of its own for every other playing at the same time.
                let astream = if claimed.contains(&clip.source) {
                    let path = src.path.clone();
                    voices.get(track_idx, clip.id, &path)
                } else {
                    claimed.push(clip.source);
                    src.audio.as_mut()
                };
                let Some(astream) = astream else {
                    continue;
                };
                mix_clip(
                    astream,
                    &clip,
                    fill_t,
                    &mut scratch[..chunk],
                    &mut mix[cursor..cursor + chunk],
                );
            }
            cursor += chunk;
            fill_t += (chunk / CHANNELS) as f64 / SAMPLE_RATE as f64;
        }
    }
}

/// The timeline time being heard at `now`. Stopped, that is the playhead
/// itself. Playing, it runs on from the last block the device took, at the
/// moment the device said that block would sound: never back before where
/// playback began, nor past what the device has been given.
fn heard(shared: &Shared, now: Instant) -> f64 {
    if !shared.playing {
        return shared.consume_t;
    }
    let Some((t0, at)) = shared.anchor else {
        return shared.play_from;
    };
    let since = if now >= at {
        now.duration_since(at).as_secs_f64()
    } else {
        -at.duration_since(now).as_secs_f64()
    };
    (t0 + since).max(shared.play_from).min(shared.consume_t)
}

/// Fade a grain in and out over [`GRAIN_FADE_SEC`] at each end, so cutting it
/// out of the middle of a sound does not click.
fn shape_grain(grain: &mut [f32]) {
    let frames = grain.len() / CHANNELS;
    let fade = ((GRAIN_FADE_SEC * SAMPLE_RATE as f64) as usize).min(frames / 2).max(1);
    for (i, frame) in grain.chunks_mut(CHANNELS).enumerate() {
        let edge = i.min(frames - 1 - i);
        if edge < fade {
            let gain = edge as f32 / fade as f32;
            frame.iter_mut().for_each(|s| *s *= gain);
        }
    }
}

/// Decode a file's audio stream end-to-end and build a peak summary. Runs on
/// the calling thread (blocks import), opens its own `Input` so it doesn't
/// interfere with the playback `AudioStream`, and resamples to packed mono f32
/// so scanning for peaks is a straight slice walk.
///
/// Returns `Ok(None)` for files without audio. `Err` only for actual decode
/// failures — callers should treat that as "no waveform" and keep the clip.
pub fn build_waveform(path: &str) -> Result<Option<Waveform>, ffmpeg::Error> {
    let mut ictx = ffmpeg::format::input(&path)?;
    let (stream_index, params) = {
        let Some(stream) = ictx.streams().best(ffmpeg::media::Type::Audio) else {
            return Ok(None);
        };
        (stream.index(), stream.parameters())
    };
    let ctx = ffmpeg::codec::context::Context::from_parameters(params)?;
    let mut decoder = ctx.decoder().audio()?;
    let rate = decoder.rate();
    let in_layout = decoder_layout(&decoder);
    let mut resampler = ffmpeg::software::resampling::Context::get(
        decoder.format(),
        in_layout,
        rate,
        ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Packed),
        ffmpeg::ChannelLayout::default(1),
        rate,
    )?;

    let bucket_samples = (rate as f64 / WAVEFORM_PEAKS_PER_SEC)
        .round()
        .max(1.0) as usize;
    let bucket_seconds = bucket_samples as f64 / rate as f64;

    let mut peaks: Vec<f32> = Vec::new();
    let mut bucket_peak = 0.0f32;
    let mut bucket_count: usize = 0;

    let absorb = |frame: &ffmpeg::frame::Audio,
                      resampler: &mut ffmpeg::software::resampling::Context,
                      peaks: &mut Vec<f32>,
                      bucket_peak: &mut f32,
                      bucket_count: &mut usize| {
        let mut resampled = ffmpeg::frame::Audio::empty();
        if resampler.run(frame, &mut resampled).is_err() {
            return;
        }
        let n = resampled.samples();
        let total_bytes = n * 4;
        let bytes = resampled.data(0);
        if bytes.len() < total_bytes {
            return;
        }
        for chunk in bytes[..total_bytes].chunks_exact(4) {
            let s = f32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            let a = s.abs();
            if a > *bucket_peak {
                *bucket_peak = a;
            }
            *bucket_count += 1;
            if *bucket_count >= bucket_samples {
                peaks.push(bucket_peak.min(1.0));
                *bucket_peak = 0.0;
                *bucket_count = 0;
            }
        }
    };

    // Feed packets, drain frames.
    {
        let mut packet_iter = ictx.packets();
        while let Some((stream, packet)) = packet_iter.next() {
            if stream.index() != stream_index {
                continue;
            }
            if decoder.send_packet(&packet).is_err() {
                continue;
            }
            let mut frame = ffmpeg::frame::Audio::empty();
            while decoder.receive_frame(&mut frame).is_ok() {
                ensure_frame_layout(&mut frame, in_layout);
                absorb(
                    &frame,
                    &mut resampler,
                    &mut peaks,
                    &mut bucket_peak,
                    &mut bucket_count,
                );
            }
        }
    }
    // Flush any remaining frames buffered inside the decoder.
    let _ = decoder.send_eof();
    let mut frame = ffmpeg::frame::Audio::empty();
    while decoder.receive_frame(&mut frame).is_ok() {
        ensure_frame_layout(&mut frame, in_layout);
        absorb(
            &frame,
            &mut resampler,
            &mut peaks,
            &mut bucket_peak,
            &mut bucket_count,
        );
    }
    if bucket_count > 0 {
        peaks.push(bucket_peak.min(1.0));
    }

    Ok(Some(Waveform {
        peaks,
        bucket_seconds,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playing_from(t: f64) -> Shared {
        Shared {
            buffer: VecDeque::new(),
            consume_t: t,
            playing: true,
            anchor: None,
            play_from: t,
            delay: 0.0,
            grain: false,
            grain_at: f64::NAN,
        }
    }

    /// The picture shows what is being heard, not what was last handed to
    /// the device: a block given over now at 10s, which the device will play
    /// in 150ms, is not heard yet.
    #[test]
    fn the_clock_runs_behind_the_device_queue() {
        let now = Instant::now();
        let mut shared = playing_from(10.0);
        assert_eq!(heard(&shared, now), 10.0, "nothing handed over yet");
        // A 20ms block from 10s, sounding in 150ms.
        shared.anchor = Some((10.0, now + std::time::Duration::from_millis(150)));
        shared.consume_t = 10.02;
        assert_eq!(heard(&shared, now), 10.0, "held at the start until it sounds");
        let later = now + std::time::Duration::from_millis(160);
        assert!((heard(&shared, later) - 10.01).abs() < 1e-6);
        // Never past what the device was given.
        let much_later = now + std::time::Duration::from_secs(1);
        assert_eq!(heard(&shared, much_later), 10.02);
    }

    /// Stopping parks the playhead on what was heard, so it does not jump on
    /// by the device's queue.
    #[test]
    fn stopping_parks_on_what_was_heard() {
        let mut shared = playing_from(5.0);
        shared.consume_t = 5.5;
        shared.anchor = Some((5.48, Instant::now() + std::time::Duration::from_secs(10)));
        AudioEngine::start_or_stop(&mut shared, false);
        assert!(!shared.playing);
        assert_eq!(shared.consume_t, 5.0);
    }

    /// A grain starts and ends silent, so it does not click, and is untouched
    /// between its fades.
    #[test]
    fn a_scrub_grain_fades_in_and_out() {
        let frames = (GRAIN_SEC * SAMPLE_RATE as f64) as usize;
        let mut grain = vec![1.0f32; frames * CHANNELS];
        shape_grain(&mut grain);
        assert_eq!(&grain[..CHANNELS], &[0.0; CHANNELS]);
        assert_eq!(&grain[grain.len() - CHANNELS..], &[0.0; CHANNELS]);
        let mid = frames / 2 * CHANNELS;
        assert_eq!(grain[mid], 1.0);
        let fade = (GRAIN_FADE_SEC * SAMPLE_RATE as f64) as usize;
        assert!(grain[(fade / 2) * CHANNELS] > 0.3 && grain[(fade / 2) * CHANNELS] < 0.7);
    }

    /// Decoding a real file is the only way to catch a source the pool accepts
    /// but cannot actually play. Needs media, so it takes a path from
    /// `RUVE_TEST_AUDIO` and skips when that is unset rather than carrying a
    /// fixture in the repo.
    #[test]
    fn two_clips_of_one_file_at_once_each_play_their_own_part() {
        let Ok(path) = std::env::var("RUVE_TEST_AUDIO") else {
            return;
        };
        // The same file twice over the same second, half a second apart in
        // it: what the mix should be is each part read on its own, added.
        let a = Clip { id: 1, source: SourceId(0), source_in: 0.0, source_out: 1.0, ..Clip::default() };
        let b = Clip { id: 2, source_in: 0.5, source_out: 1.5, ..a };
        let chunk = MIX_CHUNK_FRAMES * CHANNELS;
        let chunks = 20;
        let mut want = vec![0.0f32; chunk * chunks];
        for clip in [a, b] {
            let mut alone = AudioStream::open(&path).unwrap().unwrap();
            let mut scratch = vec![0.0f32; chunk];
            for k in 0..chunks {
                let t = (k * MIX_CHUNK_FRAMES) as f64 / SAMPLE_RATE as f64;
                mix_clip(&mut alone, &clip, t, &mut scratch, &mut want[k * chunk..(k + 1) * chunk]);
            }
        }

        // As the mixers do it: the file's reader for the first, a voice for
        // the second.
        let mut own = AudioStream::open(&path).unwrap().unwrap();
        let mut voices = Voices::default();
        let mut got = vec![0.0f32; chunk * chunks];
        let mut scratch = vec![0.0f32; chunk];
        for k in 0..chunks {
            voices.begin_round();
            let t = (k * MIX_CHUNK_FRAMES) as f64 / SAMPLE_RATE as f64;
            let out = &mut got[k * chunk..(k + 1) * chunk];
            mix_clip(&mut own, &a, t, &mut scratch, out);
            let voice = voices.get(1, b.id, &path).unwrap();
            mix_clip(voice, &b, t, &mut scratch, out);
        }
        let worst = want.iter().zip(&got).map(|(w, g)| (w - g).abs()).fold(0.0f32, f32::max);
        assert!(worst < 1e-4, "mix differs by up to {worst}");
        assert_eq!(voices.len(), 1);
        // A voice nobody asks for is closed after a couple of rounds.
        for _ in 0..4 {
            voices.begin_round();
        }
        assert_eq!(voices.len(), 0);
    }

    #[test]
    fn a_file_at_another_rate_decodes_whole_and_seeks_without_stale_audio() {
        let Ok(path) = std::env::var("RUVE_TEST_AUDIO") else {
            return;
        };
        let mut whole = AudioStream::open(&path).unwrap().unwrap();
        let mut all = vec![0.0f32; ((whole.duration() + 1.0) * SAMPLE_RATE as f64) as usize * CHANNELS];
        let n = whole.read(&mut all);
        all.truncate(n);
        let decoded = (n / CHANNELS) as f64 / SAMPLE_RATE as f64;
        // All of it, not short by what the resampler held back.
        assert!(
            (decoded - whole.duration()).abs() < 0.06,
            "decoded {decoded}s of a {}s file",
            whole.duration()
        );

        // Played a while, then sent back: what comes out is the file at the
        // new place, not what was left over from the old one.
        let mut s = AudioStream::open(&path).unwrap().unwrap();
        let mut skip = vec![0.0f32; SAMPLE_RATE as usize * CHANNELS * 3];
        s.read(&mut skip);
        let t = 0.5;
        s.seek(t);
        // And near the very start, where a file's own delay is.
        let mut early = AudioStream::open(&path).unwrap().unwrap();
        early.seek(0.01);
        let mut first = vec![0.0f32; 512 * CHANNELS];
        early.read(&mut first);
        let skip = (0.01 * SAMPLE_RATE as f64) as usize * CHANNELS;
        let off: f32 = first.iter().zip(&all[skip..]).map(|(x, y)| (x - y) * (x - y)).sum();
        let loud: f32 = all[skip..skip + first.len()].iter().map(|x| x * x).sum();
        assert!(off <= loud * 0.01 + 1e-6, "a seek near the start is off: {off} against {loud}");
        let probe = 2048;
        let mut got = vec![0.0f32; probe * CHANNELS];
        s.read(&mut got);
        // Against the straight decode at the same place, give or take the
        // few milliseconds a seek may land off by.
        let at = (t * SAMPLE_RATE as f64) as usize;
        let energy: f32 = all[at * CHANNELS..(at + probe) * CHANNELS].iter().map(|x| x * x).sum();
        let err = (0..(0.03 * SAMPLE_RATE as f64) as usize * 2)
            .map(|k| {
                let o = (at + k).saturating_sub((0.03 * SAMPLE_RATE as f64) as usize) * CHANNELS;
                got.iter().zip(&all[o..o + probe * CHANNELS]).map(|(x, y)| (x - y) * (x - y)).sum::<f32>()
            })
            .fold(f32::INFINITY, f32::min);
        assert!(energy > 0.0, "the test needs sound at {t}s");
        assert!(err < energy * 0.01, "after a seek the audio is not the file's: error {err}, signal {energy}");
    }

    #[test]
    fn an_audio_file_opens_with_a_duration_and_samples_to_read() {
        let Ok(path) = std::env::var("RUVE_TEST_AUDIO") else {
            return;
        };
        let mut stream = AudioStream::open(&path)
            .expect("open failed")
            .expect("no audio stream found");
        assert!(stream.duration() > 0.0, "duration came back as zero");

        let mut buf = vec![0.0f32; 4096];
        let n = stream.read(&mut buf);
        assert!(n > 0, "read returned no samples");
        assert!(
            buf[..n].iter().any(|s| *s != 0.0),
            "every sample read was silence"
        );
    }

    /// The timeline draws a clip's waveform from this summary, so a source that
    /// decodes but summarizes to nothing renders as a flat rect.
    #[test]
    fn an_audio_file_summarizes_to_a_waveform() {
        let Ok(path) = std::env::var("RUVE_TEST_AUDIO") else {
            return;
        };
        let wf = build_waveform(&path)
            .expect("waveform build failed")
            .expect("no audio stream found");
        assert!(!wf.peaks.is_empty(), "no peaks");
        assert!(wf.bucket_seconds > 0.0);
        assert!(wf.peaks.iter().any(|p| *p > 0.0), "every peak was zero");
    }
}
