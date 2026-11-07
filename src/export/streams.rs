//! The two encode loops, one per output stream.
//!
//! Each owns its encoder and the sources feeding it, and each is driven by the
//! muxer loop in the parent module rather than driving itself — video by frame
//! number, audio by sample count — so the two stay in step without either
//! knowing the other exists.

use std::collections::HashMap;

use ffmpeg_next as ffmpeg;
use ffmpeg::{encoder, format, frame, picture, ChannelLayout, Rational};

use crate::audio::{mix_clip, AudioStream, Voices, CHANNELS, SAMPLE_RATE};
use crate::canvas::Canvas;
use crate::compose::layers_with_compounds;
use crate::timeline::{Clip, SourceId, TrackKind, Tracks};

use super::raster::{compose, compose_masked, fill_black, place_even};
use super::take::{StillTake, VideoTake};
use super::{drain, VideoSpec};

pub(super) struct VideoRender {
    pub(super) enc: encoder::Video,
    pub(super) enc_tb: Rational,
    pub(super) stream_idx: usize,
    pub(super) stream_tb: Rational,
    pub(super) spec: VideoSpec,
    pub(super) takes: HashMap<SourceId, VideoTake>,
    /// Images, and titles drawn as images.
    pub(super) stills: HashMap<SourceId, StillTake>,
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
        compounds: &HashMap<SourceId, Tracks>,
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
        let (takes, stills) = (&self.takes, &self.stills);
        let layers = layers_with_compounds(
            tracks.iter().map(|(kind, clips)| (*kind, clips.as_slice())),
            t,
            self.canvas(),
            |source| {
                takes
                    .get(&source)
                    .map(VideoTake::size)
                    .or_else(|| stills.get(&source).map(StillTake::size))
            },
            |source| compounds.get(&source).map(Vec::as_slice),
        );
        for layer in &layers {
            let Some(placement) = place_even(layer, self.spec.width, self.spec.height) else {
                continue;
            };
            // Each source answers with the height of the picture it holds,
            // which is what the blur is worked out against — the same number
            // the preview hands the same call.
            if let Some(still) = self.stills.get_mut(&layer.source) {
                let blur = layer.blur_plan(still.size().1);
                match still.render(placement, blur) {
                    Some((picture, Some(mask))) => {
                        compose_masked(
                            &mut canvas,
                            picture,
                            mask,
                            placement.x,
                            placement.y,
                            layer.alpha,
                        );
                    }
                    Some((picture, None)) => {
                        compose(&mut canvas, picture, placement.x, placement.y, layer.alpha);
                    }
                    None => {}
                }
                continue;
            }
            let Some(take) = self.takes.get_mut(&layer.source) else {
                continue;
            };
            if !take.stage(layer.source_time) {
                continue;
            }
            let blur = layer.blur_plan(take.size().1);
            let Some(picture) = take.render(placement, blur) else {
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
    /// Readers for clips playing over another clip of the same file.
    pub(super) voices: Voices,
    /// Where each source's file is, for opening those.
    pub(super) paths: HashMap<SourceId, String>,
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
        self.voices.begin_round();
        // As live playback: the file's own reader for the first clip of it,
        // one of its own for each other playing at the same time.
        let mut claimed: Vec<SourceId> = Vec::new();
        for (track_idx, (kind, clips)) in tracks.iter().enumerate() {
            if *kind != TrackKind::Audio {
                continue;
            }
            let Some(clip) = clips.iter().find(|c| c.contains(t)) else {
                continue;
            };
            let stream = if claimed.contains(&clip.source) {
                let Some(path) = self.paths.get(&clip.source) else {
                    continue;
                };
                self.voices.get(track_idx, clip.id, path)
            } else {
                claimed.push(clip.source);
                self.takes.get_mut(&clip.source)
            };
            let Some(stream) = stream else {
                continue;
            };
            mix_clip(stream, clip, t, &mut self.scratch[..wanted], &mut self.mixed[..wanted]);
        }
    }
}
