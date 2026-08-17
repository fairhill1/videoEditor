//! The two encode loops, one per output stream.
//!
//! Each owns its encoder and the sources feeding it, and each is driven by the
//! muxer loop in the parent module rather than driving itself — video by frame
//! number, audio by sample count — so the two stay in step without either
//! knowing the other exists.

use std::collections::HashMap;

use ffmpeg_next as ffmpeg;
use ffmpeg::{encoder, format, frame, picture, ChannelLayout, Rational};

use crate::audio::{AudioStream, CHANNELS, SAMPLE_RATE};
use crate::canvas::Canvas;
use crate::compose::layers_at;
use crate::timeline::{Clip, SourceId, TrackKind};

use super::raster::{compose, compose_color, fill_black, place_even};
use super::take::{TitleTake, VideoTake};
use super::{drain, VideoSpec};

/// Re-seek an audio source once its read head drifts this far from where the
/// mix wants to sample next. Mirrors the live mixer's tolerance.
const RESEEK_THRESHOLD_SEC: f64 = 0.030;

pub(super) struct VideoRender {
    pub(super) enc: encoder::Video,
    pub(super) enc_tb: Rational,
    pub(super) stream_idx: usize,
    pub(super) stream_tb: Rational,
    pub(super) spec: VideoSpec,
    pub(super) takes: HashMap<SourceId, VideoTake>,
    pub(super) titles: HashMap<SourceId, TitleTake>,
}

impl VideoRender {
    /// The project canvas this render is filling. Built from the spec rather
    /// than carried alongside it so there is one description of the output
    /// format, not two that could disagree.
    fn canvas(&self) -> Canvas {
        Canvas {
            width: self.spec.width,
            height: self.spec.height,
            fps: self.spec.fps,
        }
    }

    pub(super) fn encode(
        &mut self,
        n: u64,
        t: f64,
        tracks: &[(TrackKind, Vec<Clip>)],
        octx: &mut format::context::Output,
    ) -> Result<(), ffmpeg::Error> {
        // A fresh frame per iteration: the encoder keeps a reference to what it
        // is handed, so reusing one buffer would let x264 read a picture we had
        // already started overwriting.
        let mut canvas = frame::Video::new(
            format::Pixel::YUV420P,
            self.spec.width,
            self.spec.height,
        );
        fill_black(&mut canvas);

        // Which pictures, where, and how much of each — decided by the same
        // code the preview asks, so the file is the frame that was approved on
        // screen rather than a second interpretation of the timeline.
        let (takes, titles) = (&self.takes, &self.titles);
        let layers = layers_at(
            tracks.iter().map(|(kind, clips)| (*kind, clips.as_slice())),
            t,
            self.canvas(),
            |source| {
                takes
                    .get(&source)
                    .map(VideoTake::size)
                    .or_else(|| titles.get(&source).map(TitleTake::size))
            },
        );
        for layer in &layers {
            let Some(placement) = place_even(layer, self.spec.width, self.spec.height) else {
                continue;
            };
            if let Some(title) = self.titles.get_mut(&layer.source) {
                let (color, color_alpha) = (title.color, title.alpha);
                if let Some(mask) = title.stage(placement) {
                    compose_color(
                        &mut canvas,
                        mask,
                        placement.x,
                        placement.y,
                        color,
                        color_alpha * layer.alpha,
                    );
                }
                continue;
            }
            let Some(take) = self.takes.get_mut(&layer.source) else {
                continue;
            };
            if !take.stage(layer.source_time) {
                continue;
            }
            let Some(picture) = take.render(placement) else {
                continue;
            };
            compose(&mut canvas, picture, placement.x, placement.y, layer.alpha);
        }

        canvas.set_pts(Some(n as i64));
        canvas.set_kind(picture::Type::None);

        self.enc.send_frame(&canvas)?;
        // One tick, because the encoder time base is 1/fps.
        drain(
            &mut self.enc,
            octx,
            self.stream_idx,
            self.enc_tb,
            self.stream_tb,
            1,
        )
    }
}

pub(super) struct AudioRender {
    pub(super) enc: encoder::Audio,
    pub(super) enc_tb: Rational,
    pub(super) stream_idx: usize,
    pub(super) stream_tb: Rational,
    pub(super) takes: HashMap<SourceId, AudioStream>,
    /// Samples per encoder frame — 1024 for AAC.
    pub(super) block: usize,
    pub(super) mixed: Vec<f32>,
    pub(super) scratch: Vec<f32>,
    pub(super) samples_done: u64,
}

impl AudioRender {
    /// Mix and encode until `target` samples have been produced in total.
    ///
    /// AAC only tolerates a short frame as the very last one, and `target`
    /// tracks video frame boundaries rather than block boundaries, so a partial
    /// tail is held back until `last` says no more samples are coming.
    pub(super) fn advance_to(
        &mut self,
        target: u64,
        last: bool,
        tracks: &[(TrackKind, Vec<Clip>)],
        octx: &mut format::context::Output,
    ) -> Result<(), ffmpeg::Error> {
        loop {
            let remaining = target.saturating_sub(self.samples_done);
            let frames = if remaining >= self.block as u64 {
                self.block
            } else if last && remaining > 0 {
                remaining as usize
            } else {
                return Ok(());
            };
            let t = self.samples_done as f64 / SAMPLE_RATE as f64;
            self.mix(t, frames, tracks);

            let mut af = frame::Audio::new(
                format::Sample::F32(format::sample::Type::Planar),
                frames,
                ChannelLayout::STEREO,
            );
            af.set_rate(SAMPLE_RATE);
            af.set_pts(Some(self.samples_done as i64));
            for ch in 0..CHANNELS {
                let plane = af.plane_mut::<f32>(ch);
                for i in 0..frames {
                    plane[i] = self.mixed[i * CHANNELS + ch];
                }
            }

            self.enc.send_frame(&af)?;
            // Always a whole block: a short final frame is padded to the
            // codec's frame size on the way out, so the packet still covers
            // `block` samples.
            drain(
                &mut self.enc,
                octx,
                self.stream_idx,
                self.enc_tb,
                self.stream_tb,
                self.block as i64,
            )?;
            self.samples_done += frames as u64;
        }
    }

    /// Sum every audio clip live at `t` into `self.mixed`, each at its own
    /// level. Same rules as live playback: tracks add together so A1 and A2
    /// both land in the render, and each clip's gain and fades ride on top —
    /// the whole reason both mixers call [`Clip::level`] rather than each
    /// deciding what a fade means.
    fn mix(&mut self, t: f64, frames: usize, tracks: &[(TrackKind, Vec<Clip>)]) {
        let wanted = frames * CHANNELS;
        self.mixed[..wanted].fill(0.0);
        for (kind, clips) in tracks {
            if *kind != TrackKind::Audio {
                continue;
            }
            let Some(clip) = clips.iter().find(|c| c.contains(t)) else {
                continue;
            };
            let Some(stream) = self.takes.get_mut(&clip.source) else {
                continue;
            };
            let src_t = clip.source_time(t);
            if (stream.position() - src_t).abs() > RESEEK_THRESHOLD_SEC {
                stream.seek(src_t);
            }
            // Zeroed first because a short read at EOF leaves the tail of
            // `scratch` holding the previous block's samples.
            self.scratch[..wanted].fill(0.0);
            let n = stream.read(&mut self.scratch[..wanted]);
            let dt = 1.0 / SAMPLE_RATE as f64;
            for i in 0..n {
                let level = clip.level(t + (i / CHANNELS) as f64 * dt);
                self.mixed[i] += self.scratch[i] * level;
            }
        }
    }
}
