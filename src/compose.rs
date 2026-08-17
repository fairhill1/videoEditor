//! What the canvas looks like at one instant, as a list of placed pictures.
//!
//! The preview and the export are two different renderers — textured quads on
//! the GPU against scaled planes on the CPU — and this is the one thing they
//! agree on. Neither decides which clips are visible, in what order, at what
//! opacity or where on the canvas; they are handed that and only choose how to
//! paint it. Before this existed both walked the timeline themselves, and the
//! only reason they agreed was that both did the same trivial thing: show the
//! topmost clip, fitted.
//!
//! Everything here is pure arithmetic over the model, so it is the same code
//! on the render thread and the export worker, and it can be tested without a
//! GPU or a decoder.

use crate::canvas::Canvas;
use crate::state::State;
use crate::timeline::{Clip, SourceId, TrackKind};

/// Below this a layer contributes nothing a viewer could see, and it is
/// cheaper to drop it than to decode a frame and blend it invisibly. One step
/// of an 8-bit channel is 1/255; half of that is comfortably under what the
/// output can represent.
const MIN_VISIBLE_ALPHA: f32 = 0.002;

/// One picture to paint onto the canvas, already placed and already told how
/// much of it to let through.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct Layer {
    pub(crate) source: SourceId,
    /// Time inside the source — what to decode, or where the generated picture
    /// is up to.
    pub(crate) source_time: f64,
    /// Destination rect in canvas pixels: `[x, y, w, h]`. Free to run outside
    /// the canvas; a renderer clips it to what it can actually paint.
    pub(crate) rect: [f32; 4],
    /// The clip's opacity with its fades applied — see [`Clip::alpha`].
    pub(crate) alpha: f32,
}

impl Layer {
    /// The part of this layer that lands inside a `width x height` canvas, as
    /// `[x, y, w, h]`, or `None` when none of it does.
    ///
    /// Both renderers need this and neither should round it its own way: a
    /// layer nudged past an edge has to lose the same strip in the preview as
    /// in the file, or the frame you approved is not the frame you get.
    pub(crate) fn visible_rect(&self, width: u32, height: u32) -> Option<[f32; 4]> {
        let [x, y, w, h] = self.rect;
        let x0 = x.max(0.0);
        let y0 = y.max(0.0);
        let x1 = (x + w).min(width as f32);
        let y1 = (y + h).min(height as f32);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        Some([x0, y0, x1 - x0, y1 - y0])
    }
}

/// Every visible picture at timeline time `t`, bottom layer first.
///
/// `size_of` reports a source's pixel dimensions, which is what the fit needs
/// and the only thing here that a caller has to look up for itself — the
/// preview asks its decoders, the export asks the files it opened. A source
/// that reports nothing is skipped rather than guessed at: it is one that
/// failed to open, and inventing a size for it would put a stretched or
/// mispositioned rectangle on the canvas instead of nothing.
///
/// Painting order is track order, so a higher track covers a lower one — the
/// arrangement the timeline already draws, with V2 sitting above V1.
pub(crate) fn layers_at<'a>(
    tracks: impl Iterator<Item = (TrackKind, &'a [Clip])>,
    t: f64,
    canvas: Canvas,
    mut size_of: impl FnMut(SourceId) -> Option<(u32, u32)>,
) -> Vec<Layer> {
    let mut layers = Vec::new();
    for (kind, clips) in tracks {
        if kind != TrackKind::Video {
            continue;
        }
        // One clip per track: clips on a track are meant not to overlap, and
        // where a stray edit has left two stacked, taking the first matches
        // what the audio mixer does with the same situation.
        let Some(clip) = clips.iter().find(|c| c.contains(t)) else {
            continue;
        };
        let alpha = clip.alpha(t);
        if alpha < MIN_VISIBLE_ALPHA {
            continue;
        }
        let Some((sw, sh)) = size_of(clip.source) else {
            continue;
        };
        layers.push(Layer {
            source: clip.source,
            source_time: clip.source_time(t),
            rect: canvas.place(sw as f32, sh as f32, clip.transform),
            alpha,
        });
    }
    layers
}

impl State {
    /// Pixel dimensions of a source's picture, or `None` for one that has no
    /// picture — an audio-only import, or a file whose video stream failed to
    /// open.
    ///
    /// A title's picture is the canvas: it is drawn at whatever size the
    /// project is, which is what makes its neutral transform fill the frame the
    /// same way a matching clip's does.
    pub(crate) fn source_size(&self, id: SourceId, canvas: Canvas) -> Option<(u32, u32)> {
        let src = self.media.get(id)?;
        if src.title.is_some() {
            return Some((canvas.width, canvas.height));
        }
        let stream = src.stream.as_ref()?;
        Some((stream.width(), stream.height()))
    }

    /// The layers the preview should paint at `t`. The export builds the same
    /// list from its own snapshot of the timeline and its own decoders.
    pub(crate) fn frame_layers(&self, t: f64, canvas: Canvas) -> Vec<Layer> {
        let tracks: Vec<(TrackKind, &[Clip])> = self
            .timeline
            .tracks
            .iter()
            .map(|tr| (tr.kind, tr.clips.as_slice()))
            .collect();
        layers_at(tracks.into_iter(), t, canvas, |id| {
            self.source_size(id, canvas)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::Transform;

    fn canvas() -> Canvas {
        Canvas { width: 1920, height: 1080, fps: 25.0 }
    }

    fn clip(source: u32, start: f64, len: f64) -> Clip {
        Clip {
            source: SourceId(source),
            source_out: len,
            timeline_start: start,
            ..Clip::default()
        }
    }

    /// Video tracks are stacked bottom-first, so the list has to come back in
    /// the order a painter would use — get this backwards and the lower track
    /// covers the higher one.
    #[test]
    fn layers_come_back_bottom_track_first() {
        let v1 = [clip(0, 0.0, 10.0)];
        let v2 = [clip(1, 0.0, 10.0)];
        let layers = layers_at(
            [
                (TrackKind::Video, &v1[..]),
                (TrackKind::Video, &v2[..]),
            ]
            .into_iter(),
            1.0,
            canvas(),
            |_| Some((1920, 1080)),
        );
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0].source, SourceId(0));
        assert_eq!(layers[1].source, SourceId(1));
    }

    #[test]
    fn audio_tracks_contribute_no_picture() {
        let a1 = [clip(0, 0.0, 10.0)];
        let layers = layers_at(
            [(TrackKind::Audio, &a1[..])].into_iter(),
            1.0,
            canvas(),
            |_| Some((1920, 1080)),
        );
        assert!(layers.is_empty());
    }

    /// A clip fully faded out is not a black rectangle over what is beneath it
    /// — it is not there at all, which is what makes an overlapping pair a
    /// dissolve rather than a flash of black.
    #[test]
    fn a_clip_faded_to_nothing_drops_out_of_the_frame() {
        let mut c = clip(0, 0.0, 10.0);
        c.fade_out = 2.0;
        let track = [c];
        let at = |t: f64| {
            layers_at(
                [(TrackKind::Video, &track[..])].into_iter(),
                t,
                canvas(),
                |_| Some((1920, 1080)),
            )
        };
        assert_eq!(at(9.0)[0].alpha, 0.5);
        assert!(at(10.0 - 1e-9).is_empty());
    }

    /// Opacity and the fades multiply: a clip held at half strength and then
    /// faded reaches a quarter half way through the ramp, not a half.
    #[test]
    fn opacity_and_fades_compound() {
        let mut c = clip(0, 0.0, 10.0);
        c.opacity = 0.5;
        c.fade_in = 4.0;
        let track = [c];
        let layers = layers_at(
            [(TrackKind::Video, &track[..])].into_iter(),
            2.0,
            canvas(),
            |_| Some((1920, 1080)),
        );
        assert_eq!(layers[0].alpha, 0.25);
    }

    #[test]
    fn a_source_that_never_opened_is_skipped_rather_than_placed() {
        let track = [clip(7, 0.0, 10.0)];
        let layers = layers_at(
            [(TrackKind::Video, &track[..])].into_iter(),
            1.0,
            canvas(),
            |_| None,
        );
        assert!(layers.is_empty());
    }

    /// The default transform has to reproduce the fit exactly, or every
    /// project made before clips could be placed shifts the first time it is
    /// opened.
    #[test]
    fn an_untransformed_clip_lands_where_the_plain_fit_puts_it() {
        let c = canvas();
        assert_eq!(c.place(1920.0, 1080.0, Transform::default()), [0.0, 0.0, 1920.0, 1080.0]);
        assert_eq!(c.place(1080.0, 1080.0, Transform::default()), [420.0, 0.0, 1080.0, 1080.0]);
    }

    /// Quarter-size in the bottom-right corner — the picture-in-picture the
    /// whole transform exists for.
    #[test]
    fn scale_shrinks_about_the_clips_own_centre() {
        let c = canvas();
        let tf = Transform { x: 0.25, y: 0.25, scale: 0.5 };
        assert_eq!(c.place(1920.0, 1080.0, tf), [960.0, 540.0, 960.0, 540.0]);
    }

    /// Offsets are fractions of the canvas, so the same project re-mastered to
    /// a larger canvas keeps its overlays in the same place rather than in the
    /// same pixel.
    #[test]
    fn placement_is_resolution_independent() {
        let hd = Canvas { width: 1920, height: 1080, fps: 25.0 };
        let uhd = Canvas { width: 3840, height: 2160, fps: 25.0 };
        let tf = Transform { x: 0.1, y: -0.2, scale: 0.5 };
        let a = hd.place(1920.0, 1080.0, tf);
        let b = uhd.place(1920.0, 1080.0, tf);
        for i in 0..4 {
            assert!((a[i] * 2.0 - b[i]).abs() < 1e-3, "component {i}: {a:?} vs {b:?}");
        }
    }

    #[test]
    fn a_layer_pushed_off_an_edge_keeps_only_what_is_on_screen() {
        let layer = Layer {
            source: SourceId(0),
            source_time: 0.0,
            rect: [-100.0, -50.0, 400.0, 200.0],
            alpha: 1.0,
        };
        assert_eq!(layer.visible_rect(1920, 1080), Some([0.0, 0.0, 300.0, 150.0]));
    }

    #[test]
    fn a_layer_entirely_off_canvas_is_not_visible_at_all() {
        let layer = Layer {
            source: SourceId(0),
            source_time: 0.0,
            rect: [-400.0, 0.0, 400.0, 200.0],
            alpha: 1.0,
        };
        assert_eq!(layer.visible_rect(1920, 1080), None);
    }
}
