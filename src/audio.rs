use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ffmpeg_next as ffmpeg;

use crate::media::MediaPool;
use crate::timeline::{Timeline, TrackKind};

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

/// Streaming audio decoder for one source file. Mirrors `VideoStream`: opens its
/// own ffmpeg `Input` so seeks are independent of the video side, resamples into
/// interleaved stereo f32 at `SAMPLE_RATE`, and keeps a small pending buffer so
/// callers can pull arbitrary counts.
pub struct AudioStream {
    ictx: ffmpeg::format::context::Input,
    decoder: ffmpeg::decoder::Audio,
    resampler: ffmpeg::software::resampling::Context,
    stream_index: usize,
    time_base_seconds: f64,
    duration_seconds: f64,
    pending: VecDeque<f32>,
    /// Timeline time of the next sample to be returned by `read`.
    position: f64,
    eof: bool,
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

        let (stream_index, time_base_seconds, parameters) = {
            let Some(input) = ictx.streams().best(ffmpeg::media::Type::Audio) else {
                return Ok(None);
            };
            let tb = input.time_base();
            (
                input.index(),
                tb.numerator() as f64 / tb.denominator() as f64,
                input.parameters(),
            )
        };

        let ctx = ffmpeg::codec::context::Context::from_parameters(parameters)?;
        let decoder = ctx.decoder().audio()?;

        let in_layout = {
            let l = decoder.channel_layout();
            if l.bits() != 0 {
                l
            } else {
                ffmpeg::ChannelLayout::default(decoder.channels() as i32)
            }
        };

        let resampler = ffmpeg::software::resampling::Context::get(
            decoder.format(),
            in_layout,
            decoder.rate(),
            ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Packed),
            ffmpeg::ChannelLayout::STEREO,
            SAMPLE_RATE,
        )?;

        Ok(Some(Self {
            ictx,
            decoder,
            resampler,
            stream_index,
            time_base_seconds,
            duration_seconds,
            pending: VecDeque::new(),
            position: 0.0,
            eof: false,
        }))
    }

    pub fn duration(&self) -> f64 {
        self.duration_seconds
    }

    pub fn position(&self) -> f64 {
        self.position
    }

    /// Seek so that subsequent `read` calls begin at `t`. Demuxer lands on the
    /// nearest keyframe before `t`; we drop decoded samples from the head to
    /// bring the stream exactly to `t`.
    pub fn seek(&mut self, t: f64) {
        let t = t.max(0.0);
        let ts = (t * AV_TIME_BASE) as i64;
        let _ = self.ictx.seek(ts, ..);
        self.decoder.flush();
        self.pending.clear();
        self.position = t;
        self.eof = false;

        // Prime one frame so we know where the demuxer actually landed, and
        // drop samples to align to `t`.
        let Some(first_pts) = self.decode_into_pending() else {
            return;
        };
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
                    let pts = frame.pts().unwrap_or(0);
                    let pts_sec = pts as f64 * self.time_base_seconds;
                    let mut resampled = ffmpeg::frame::Audio::empty();
                    if self.resampler.run(&frame, &mut resampled).is_err() {
                        return Some(pts_sec);
                    }
                    let n_frames = resampled.samples();
                    let n_samples = n_frames * CHANNELS;
                    let total_bytes = n_samples * 4;
                    let bytes = resampled.data(0);
                    if bytes.len() >= total_bytes {
                        for chunk in bytes[..total_bytes].chunks_exact(4) {
                            let s = f32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                            self.pending.push_back(s);
                        }
                    }
                    return Some(pts_sec);
                }
                Err(_) => match self.next_audio_packet() {
                    Some(packet) => {
                        let _ = self.decoder.send_packet(&packet);
                    }
                    None => {
                        let _ = self.decoder.send_eof();
                        self.eof = true;
                        return None;
                    }
                },
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

// ---------------------------------------------------------------------------

struct Shared {
    /// Interleaved stereo f32 at `SAMPLE_RATE`.
    buffer: VecDeque<f32>,
    /// Timeline time of the next sample the cpal callback will consume.
    /// Advances in real time when `playing`, and is the master clock everyone
    /// else (video render, scrub) reads.
    consume_t: f64,
    playing: bool,
}

/// Owns the cpal output stream and the mixer. The cpal callback on the audio
/// thread drains `Shared::buffer` and advances `consume_t`; the render thread
/// calls `tick` once per frame to refill the buffer from the current timeline.
pub struct AudioEngine {
    _stream: Option<cpal::Stream>,
    shared: Arc<Mutex<Shared>>,
}

impl AudioEngine {
    pub fn new() -> Self {
        let shared = Arc::new(Mutex::new(Shared {
            buffer: VecDeque::with_capacity(SAMPLE_RATE as usize * CHANNELS),
            consume_t: 0.0,
            playing: false,
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
        }
    }

    fn build_stream(shared: &Arc<Mutex<Shared>>) -> Result<cpal::Stream, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "no default output device".to_string())?;
        let supported = device
            .default_output_config()
            .map_err(|e| e.to_string())?;

        let device_channels = supported.channels() as usize;
        let device_rate = supported.sample_rate();

        // We mix at SAMPLE_RATE; if the device wants a different rate, sample
        // rate conversion would belong here. For MVP rely on the OS accepting
        // our rate — it almost always does for 48kHz on modern Linux/Mac/Win.
        if device_rate != SAMPLE_RATE {
            log::warn!(
                "audio device rate {device_rate} != {SAMPLE_RATE}; playback may pitch-shift"
            );
        }

        if supported.sample_format() != cpal::SampleFormat::F32 {
            return Err(format!(
                "unsupported device sample format: {:?}",
                supported.sample_format()
            ));
        }

        let config: cpal::StreamConfig = supported.into();
        let shared_cb = shared.clone();
        let stream = device
            .build_output_stream(
                &config,
                move |out: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                    let mut shared = shared_cb.lock().unwrap();
                    if shared.playing {
                        for frame_slot in out.chunks_mut(device_channels) {
                            let l = shared.buffer.pop_front().unwrap_or(0.0);
                            let r = if CHANNELS > 1 {
                                shared.buffer.pop_front().unwrap_or(l)
                            } else {
                                l
                            };
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
                        // Advance by the whole block duration — if we underran
                        // and wrote silence, time still marched on.
                        let frames = out.len() / device_channels.max(1);
                        shared.consume_t += frames as f64 / device_rate as f64;
                    } else {
                        for s in out.iter_mut() {
                            *s = 0.0;
                        }
                    }
                },
                |err| log::error!("audio stream error: {err}"),
                None,
            )
            .map_err(|e| e.to_string())?;

        stream.play().map_err(|e| e.to_string())?;
        Ok(stream)
    }

    pub fn position(&self) -> f64 {
        self.shared.lock().unwrap().consume_t
    }

    /// Jump the playhead. Drops any pre-mixed samples so the next `tick` refills
    /// from the new time — scrub/seek should feel immediate, not drift through
    /// 150ms of stale buffer.
    pub fn set_position(&self, t: f64) {
        let mut shared = self.shared.lock().unwrap();
        shared.consume_t = t.max(0.0);
        shared.buffer.clear();
    }

    pub fn playing(&self) -> bool {
        self.shared.lock().unwrap().playing
    }

    pub fn set_playing(&self, playing: bool) {
        let mut shared = self.shared.lock().unwrap();
        shared.playing = playing;
        if !playing {
            shared.buffer.clear();
        }
    }

    pub fn toggle(&self) {
        let mut shared = self.shared.lock().unwrap();
        shared.playing = !shared.playing;
        if !shared.playing {
            shared.buffer.clear();
        }
    }

    /// Refill the output buffer up to `TARGET_BUFFER_SEC` ahead of the current
    /// consume point. Called from the render loop — cheap when there's already
    /// plenty buffered, does actual decode work when there isn't.
    pub fn tick(&self, timeline: &Timeline, media: &mut MediaPool) {
        let (mut fill_t, to_produce) = {
            let shared = self.shared.lock().unwrap();
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
        let mut scratch = vec![0.0f32; MIX_CHUNK_FRAMES * CHANNELS];

        let mut cursor = 0;
        while cursor < to_produce {
            let chunk = (to_produce - cursor).min(MIX_CHUNK_FRAMES * CHANNELS);
            // Sum every audio clip that's live at `fill_t`. Multi-track mixing
            // is the editor-standard behavior — A1 + A2 both play.
            for track_idx in 0..timeline.tracks.len() {
                let track = &timeline.tracks[track_idx];
                if track.kind != TrackKind::Audio {
                    continue;
                }
                let Some(clip) = track.active_clip(fill_t) else {
                    continue;
                };
                let src_t = clip.source_in + (fill_t - clip.timeline_start).max(0.0);
                let source_id = clip.source;
                let Some(src) = media.get_mut(source_id) else {
                    continue;
                };
                let Some(astream) = src.audio.as_mut() else {
                    continue;
                };
                if (astream.position() - src_t).abs() > RESEEK_THRESHOLD_SEC {
                    astream.seek(src_t);
                }
                // Zero scratch before read — read may short-write at EOF.
                for s in scratch[..chunk].iter_mut() {
                    *s = 0.0;
                }
                let n = astream.read(&mut scratch[..chunk]);
                for i in 0..n {
                    mix[cursor + i] += scratch[i];
                }
            }
            cursor += chunk;
            fill_t += (chunk / CHANNELS) as f64 / SAMPLE_RATE as f64;
        }

        let mut shared = self.shared.lock().unwrap();
        shared.buffer.extend(mix.iter().copied());
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
    let in_layout = {
        let l = decoder.channel_layout();
        if l.bits() != 0 {
            l
        } else {
            ffmpeg::ChannelLayout::default(decoder.channels() as i32)
        }
    };
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

    let mut absorb = |frame: &ffmpeg::frame::Audio,
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
