//! Where things are, and what the cursor is over.
//!
//! Everything here is geometry: resolving the panel splits and the timeline's
//! visible window, mapping between time and x, finding the lane under a y, and
//! hit-testing the media pool. Drawing lives elsewhere and asks these questions
//! rather than answering them again, which is what keeps a rect you can click
//! on and the rect you can see from drifting apart.

use crate::input::DragMode;
use crate::state::State;
use crate::theme::*;
use crate::timeline::{
    db_to_gain, gain_to_db, Clip, FadeSide, SourceId, TrackKind, MAX_GAIN_DB, MAX_OPACITY,
    MIN_GAIN_DB, MIN_OPACITY,
};
use crate::ui::Rect;

/// The two panel dividers, each named for what it separates.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) enum Splitter {
    /// Horizontal, between the timeline and everything above it.
    TopBottom,
    /// Vertical, between the media pool and the preview.
    PoolPreview,
}

/// Resolve a stored split fraction into a position in points, honouring the
/// minimum size of the panel on either side.
///
/// Splits are kept as fractions so a resized window keeps its proportions, but
/// a fraction alone will happily squeeze a panel to nothing on a small window.
/// Clamping here — on every read, not just while dragging — means a window
/// shrunk past a minimum and grown again finds its split where it left it.
pub(crate) fn resolve_split(frac: f32, total: f32, min_before: f32, min_after: f32) -> f32 {
    if total <= min_before + min_after {
        // Not enough room for both minimums. Divide what there is in the ratio
        // of the minimums themselves: neither panel vanishes, and the result is
        // stable rather than depending on which one we chose to satisfy first.
        return (total * min_before / (min_before + min_after)).round();
    }
    (frac * total).round().clamp(min_before, total - min_after)
}

/// Shortest stretch of timeline the view can be zoomed down to. At a typical
/// panel width that is a few hundred pixels per second — fine enough to place
/// a cut between two frames at any sane frame rate, and a floor rather than
/// nothing at all so a wheel spin can't divide the span to zero.
pub(crate) const MIN_VIEW_SPAN: f64 = 0.1;

/// How much timeline an empty project pretends to have, so that the mapping
/// from time to x is finite before the first clip lands.
pub(crate) const MIN_CONTENT_SPAN: f64 = 1.0;

/// How much of a wheel notch is: one step multiplies or divides the visible
/// span by this.
const ZOOM_STEP: f64 = 1.2;

/// How far one notch pans, as a fraction of the visible span. A fraction
/// rather than a duration so the gesture covers the same visible distance at
/// every zoom.
const PAN_STEP_FRAC: f64 = 0.12;

/// Where the playhead lands in the view when playback pages it forward, as a
/// fraction of the visible span. A little in from the left edge, so the frame
/// the page happened on is still visible next to what follows it.
const VIEW_FOLLOW_LEAD: f64 = 0.1;

/// Resolve a requested view window against the content there is to look at.
///
/// `want_dur` is what was asked for, not what is shown: any value at or above
/// the content resolves to the whole of it, which is what makes fit-to-content
/// both the state the editor opens in (`f64::INFINITY`) and the state zooming
/// all the way out returns to. Storing the request rather than the result is
/// what lets a clip added later widen a fitted view instead of scrolling off
/// the end of it.
pub(crate) fn resolve_view(want_start: f64, want_dur: f64, content: f64) -> (f64, f64) {
    let content = content.max(MIN_CONTENT_SPAN);
    let dur = want_dur.clamp(MIN_VIEW_SPAN, content);
    (want_start.clamp(0.0, content - dur), dur)
}

pub(crate) enum TimelineHit {
    None,
    Ruler,
    Lane,
    ClipBody { track: usize, idx: usize, grab_t_offset: f64 },
    ClipTrimLeft { track: usize, idx: usize },
    ClipTrimRight { track: usize, idx: usize },
    ClipFade { track: usize, idx: usize, side: FadeSide },
    ClipLevel { track: usize, idx: usize },
}

#[derive(Copy, Clone)]
pub(crate) struct TimelineLayout {
    pub(crate) top: f32,
    pub(crate) clips_x: f32,
    pub(crate) clips_w: f32,
    pub(crate) center_y: f32,
    pub(crate) lane_h: f32,
    /// Bottom edge of the panel, which is the window's.
    pub(crate) bottom: f32,
    /// The stretch of timeline the clip area shows, resolved by
    /// [`resolve_view`]. Time maps to x through these two and nothing else, so
    /// zooming is a change to them rather than to every site that draws.
    pub(crate) view_start: f64,
    pub(crate) view_dur: f64,
    /// How much timeline there is in total — what the view is a window onto,
    /// and so what the scrollbar measures itself against.
    pub(crate) content: f64,
}

impl TimelineLayout {
    /// Clamped to the clip area, so a drag that runs off either end of the
    /// panel pins to the edge of what is visible rather than reading a time
    /// from underneath the track headers.
    pub(crate) fn cursor_to_t(&self, cursor_x: f32) -> f64 {
        let ratio = ((cursor_x - self.clips_x) / self.clips_w).clamp(0.0, 1.0) as f64;
        self.view_start + ratio * self.view_dur
    }

    pub(crate) fn t_to_x(&self, t: f64) -> f32 {
        self.clips_x + ((t - self.view_start) / self.view_dur) as f32 * self.clips_w
    }

    /// Points per second at the current zoom. The one conversion for anything
    /// authored in pixels that has to act on a duration — a snap threshold, the
    /// width of a fade.
    pub(crate) fn px_per_sec(&self) -> f64 {
        self.clips_w as f64 / self.view_dur
    }

    pub(crate) fn clips_right(&self) -> f32 {
        self.clips_x + self.clips_w
    }

    /// The strip along the bottom of the panel the scrollbar lives in. It
    /// spans the clip area and nothing else: it measures the same axis those
    /// clips are laid out on, and starting it under the track headers would
    /// put its ends somewhere other than the timeline's.
    pub(crate) fn scrollbar_rect(&self) -> Rect {
        Rect {
            x: self.clips_x,
            y: self.bottom - TIMELINE_SCROLLBAR_H,
            w: self.clips_w,
            h: TIMELINE_SCROLLBAR_H,
        }
    }

    /// The thumb, or `None` when the view already spans the whole timeline and
    /// there is nothing to scroll.
    pub(crate) fn scroll_thumb(&self) -> Option<Rect> {
        if self.view_dur >= self.content {
            return None;
        }
        let track = self.scrollbar_rect();
        let w = (track.w * (self.view_dur / self.content) as f32).max(SCROLLBAR_MIN_THUMB_W);
        let travel = (track.w - w).max(0.0);
        let along = (self.view_start / (self.content - self.view_dur)).clamp(0.0, 1.0) as f32;
        Some(Rect {
            x: track.x + travel * along,
            y: track.y + SCROLLBAR_THUMB_INSET,
            w,
            h: (track.h - SCROLLBAR_THUMB_INSET * 2.0).max(1.0),
        })
    }

    /// Where along the thumb a press at `cursor_x` takes hold of it, or `None`
    /// when there is no thumb to take.
    ///
    /// A press beside the thumb grabs it by the middle, which jumps the view to
    /// the press and leaves the drag that follows tracking the cursor from
    /// where it already is — one gesture rather than a jump you then have to
    /// chase.
    pub(crate) fn scroll_grab_offset(&self, cursor_x: f32) -> Option<f32> {
        let thumb = self.scroll_thumb()?;
        Some(if cursor_x >= thumb.x && cursor_x <= thumb.x + thumb.w {
            cursor_x - thumb.x
        } else {
            thumb.w * 0.5
        })
    }

    /// The view start a thumb whose left edge is at `thumb_x` means — the
    /// inverse of [`TimelineLayout::scroll_thumb`], and the only place a drag
    /// on the scrollbar turns back into a time.
    pub(crate) fn scroll_x_to_view_start(&self, thumb_x: f32) -> f64 {
        let Some(thumb) = self.scroll_thumb() else {
            return 0.0;
        };
        let track = self.scrollbar_rect();
        let travel = track.w - thumb.w;
        if travel <= 0.0 {
            return 0.0;
        }
        let along = ((thumb_x - track.x) / travel).clamp(0.0, 1.0) as f64;
        along * (self.content - self.view_dur)
    }
}

/// Top edge of the lane `visual_i` places into, counting within its own kind.
///
/// Video stacks upward from the V/A divider and audio downward, each starting
/// half a gap clear of it. Drawing and hit testing both come through here, so a
/// lane you can click on is the lane you can see.
pub(crate) fn lane_y(center_y: f32, lane_h: f32, visual_i: usize, kind: TrackKind) -> f32 {
    let half_gap = TRACK_LANE_GAP * 0.5;
    let stride = lane_h + TRACK_LANE_GAP;
    match kind {
        TrackKind::Video => center_y - half_gap - lane_h - visual_i as f32 * stride,
        TrackKind::Audio => center_y + half_gap + visual_i as f32 * stride,
    }
}

/// The stretch of a lane the level line travels, as (top, height).
///
/// Inset at both ends so the extremes of the range stay clear of the clip's own
/// border — a line sitting exactly on it would be impossible to tell from it,
/// and impossible to grab without catching the trim handle instead.
fn level_band(lane_y: f32, lane_h: f32) -> (f32, f32) {
    (
        lane_y + CLIP_LEVEL_INSET,
        (lane_h - CLIP_LEVEL_INSET * 2.0).max(1.0),
    )
}

/// Where a clip's level line sits inside its lane.
///
/// Linear in decibels rather than in the stored linear gain, which is what puts
/// unity near the top of the lane: the range runs from `MIN_GAIN_DB` to
/// `MAX_GAIN_DB`, and only 6 of those 46 dB are above unity. That matches what
/// the control is for — attenuating is the common move, and it gets the room.
pub(crate) fn gain_to_y(lane_y: f32, lane_h: f32, gain: f32) -> f32 {
    let (top, h) = level_band(lane_y, lane_h);
    let db = gain_to_db(gain).clamp(MIN_GAIN_DB, MAX_GAIN_DB);
    top + h * (MAX_GAIN_DB - db) / (MAX_GAIN_DB - MIN_GAIN_DB)
}

/// Where a video clip's opacity line sits inside its lane.
///
/// Linear, unlike the audio line's decibels. Opacity has no headroom above
/// unity and no floor to compress towards, so half way down the lane is half
/// the picture — a scale nobody has to learn, and one that puts the fully
/// transparent end where the fully silent end already is.
pub(crate) fn opacity_to_y(lane_y: f32, lane_h: f32, opacity: f32) -> f32 {
    let (top, h) = level_band(lane_y, lane_h);
    let o = opacity.clamp(MIN_OPACITY, MAX_OPACITY);
    top + h * (MAX_OPACITY - o) / (MAX_OPACITY - MIN_OPACITY)
}

/// Inverse of [`opacity_to_y`], clamped like [`y_to_gain`].
pub(crate) fn y_to_opacity(lane_y: f32, lane_h: f32, y: f32) -> f32 {
    let (top, h) = level_band(lane_y, lane_h);
    let frac = ((y - top) / h).clamp(0.0, 1.0);
    MAX_OPACITY - frac * (MAX_OPACITY - MIN_OPACITY)
}

/// Where a clip's level line sits, whichever quantity its track puts on it —
/// the sound's level on an audio lane, the picture's opacity on a video one.
///
/// Both lines are the same control and the same gesture, so drawing and hit
/// testing come through one function rather than each deciding for itself which
/// of the two a lane carries.
pub(crate) fn level_line_y(kind: TrackKind, lane_y: f32, lane_h: f32, clip: &Clip) -> f32 {
    match kind {
        TrackKind::Audio => gain_to_y(lane_y, lane_h, clip.gain),
        TrackKind::Video => opacity_to_y(lane_y, lane_h, clip.opacity),
    }
}

/// Inverse of [`gain_to_y`]. Clamped, so a drag that runs off the lane pins to
/// the end of the range rather than wrapping past it.
pub(crate) fn y_to_gain(lane_y: f32, lane_h: f32, y: f32) -> f32 {
    let (top, h) = level_band(lane_y, lane_h);
    let frac = ((y - top) / h).clamp(0.0, 1.0);
    db_to_gain(MAX_GAIN_DB - frac * (MAX_GAIN_DB - MIN_GAIN_DB))
}

/// The box you grab to drag a fade, given where the fade currently ends.
///
/// Centered on that point but never hanging outside the clip, so a fade of zero
/// puts its handle just inside the clip's corner instead of half over the
/// neighbour. It only claims the top of the lane: the rest of the clip edge
/// stays a trim handle, which is the more common gesture and the worse one to
/// lose.
pub(crate) fn fade_handle_rect(handle_x: f32, x0: f32, x1: f32, lane_y: f32) -> Rect {
    let box_w = CLIP_FADE_HANDLE_BOX;
    let x = (handle_x - box_w * 0.5).clamp(x0, (x1 - box_w).max(x0));
    Rect {
        x,
        y: lane_y,
        w: box_w,
        h: box_w,
    }
}

/// The four corner boxes that scale a clip's picture on the canvas.
///
/// Pulled inside the frame rather than centred on its corners: a picture placed
/// flush against an edge of the canvas would otherwise put half of each handle
/// outside the preview, and the half that is left is the half nobody aims at.
pub(crate) fn transform_handle_rects(rect: Rect) -> [Rect; 4] {
    let b = TRANSFORM_HANDLE_BOX.min(rect.w).min(rect.h);
    HANDLE_CORNERS.map(|[cx, cy]| Rect {
        x: rect.x + cx * (rect.w - b),
        y: rect.y + cy * (rect.h - b),
        w: b,
        h: b,
    })
}

/// The corners a picture can be scaled by, as fractions of its own rect: `0.0`
/// for the left or top edge and `1.0` for the right or bottom.
///
/// Kept as fractions rather than as a named enum because that is the form the
/// arithmetic wants — the corner held still is `1.0 - corner`, and where each
/// edge lands falls straight out of the pair.
const HANDLE_CORNERS: [[f32; 2]; 4] = [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]];

/// What a press on the preview canvas has taken hold of.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) enum PreviewHit {
    /// A corner box, named by which corner it is. Dragging it scales the
    /// picture with the opposite corner pinned, the way a corner handle behaves
    /// anywhere else — the edge under the hand is the edge that moves.
    Scale { corner: [f32; 2] },
    /// The picture itself: move it around the canvas.
    Move,
}

/// Top edge of the highest video lane. Video stacks upward from the V/A
/// divider, so this is where the ruler has to end.
pub(crate) fn topmost_lane_top(center_y: f32, lane_h: f32, n_video: usize) -> f32 {
    if n_video == 0 {
        center_y
    } else {
        lane_y(center_y, lane_h, n_video - 1, TrackKind::Video)
    }
}

pub(crate) fn compute_lane_height(tracks_area_h: f32, n_tracks: usize) -> f32 {
    let n = n_tracks.max(1) as f32;
    let gaps = (n - 1.0).max(0.0) * TRACK_LANE_GAP;
    let avail = (tracks_area_h * TRACK_LANE_FILL - gaps).max(0.0);
    (avail / n)
        .clamp(TRACK_LANE_MIN_H, TRACK_LANE_MAX_H)
        .round()
}

pub(crate) fn pool_row_close_rect(row_x: f32, row_y: f32, row_w: f32) -> Rect {
    Rect {
        x: row_x + row_w - POOL_CLOSE_INSET - POOL_CLOSE_BOX,
        y: row_y + POOL_CLOSE_INSET,
        w: POOL_CLOSE_BOX,
        h: POOL_CLOSE_BOX,
    }
}

impl State {
    /// Window size in logical points — the coordinate space all layout, hit
    /// testing and drawing is written in. See [`State::scale`].
    pub(crate) fn logical_size(&self) -> [f32; 2] {
        [
            self.size.width as f32 / self.scale,
            self.size.height as f32 / self.scale,
        ]
    }

    /// Y of the divider between the timeline and the panels above it.
    pub(crate) fn timeline_top(&self) -> f32 {
        let h = self.logical_size()[1];
        resolve_split(self.split_top_bottom, h, TOP_MIN_H, TIMELINE_MIN_H)
    }

    /// Width of the media pool, i.e. X of the divider between it and the preview.
    pub(crate) fn media_pool_w(&self) -> f32 {
        let w = self.logical_size()[0];
        resolve_split(self.split_pool_preview, w, POOL_MIN_W, PREVIEW_MIN_W)
    }

    /// Which divider, if any, the cursor is close enough to grab.
    ///
    /// The horizontal one is tested first and spans the full width, so at the
    /// T-junction where the two meet it wins. Either answer is defensible
    /// there; what matters is that the hover highlight and the press agree,
    /// which they do by both coming through here.
    pub(crate) fn splitter_at(&self, [cx, cy]: [f32; 2]) -> Option<Splitter> {
        let top = self.timeline_top();
        if (cy - top).abs() <= SPLITTER_GRAB_PX {
            return Some(Splitter::TopBottom);
        }
        if cy < top && (cx - self.media_pool_w()).abs() <= SPLITTER_GRAB_PX {
            return Some(Splitter::PoolPreview);
        }
        None
    }

    pub(crate) fn timeline_layout(&self) -> TimelineLayout {
        let [w, bottom] = self.logical_size();
        let top = self.timeline_top();
        let tracks_top = top + TIMELINE_TOP_PAD;
        // The scroll strip's height comes off the lanes' share, so the bottom
        // lane has somewhere to end that isn't underneath the scrollbar.
        let tracks_area_h = (bottom - tracks_top - TIMELINE_SCROLLBAR_H).max(0.0);
        let (view_start, view_dur) = self.view_window();
        TimelineLayout {
            top,
            bottom,
            clips_x: TRACK_HEADER_WIDTH,
            clips_w: (w - TRACK_HEADER_WIDTH).max(1.0),
            // Snap the centre to a whole point so the lane edges derived from
            // it don't land on halves, which render blurred.
            center_y: (tracks_top + tracks_area_h * 0.5).round(),
            lane_h: compute_lane_height(tracks_area_h, self.timeline.tracks.len()),
            view_start,
            view_dur,
            content: self.content_duration(),
        }
    }

    /// How much timeline there is to look at.
    ///
    /// A clip being dragged in from the pool counts towards it, so a fitted
    /// view has already made room for the drop by the time the ghost is drawn:
    /// without that, dragging a thirty-second clip onto an empty timeline would
    /// preview it thirty times the width of the panel it is about to land in.
    pub(crate) fn content_duration(&self) -> f64 {
        let incoming = match self.drag {
            DragMode::PoolDrag { source } => self.media.duration(source),
            _ => 0.0,
        };
        self.timeline.duration().max(incoming).max(MIN_CONTENT_SPAN)
    }

    /// The stretch of timeline currently on screen, as `(start, duration)`.
    pub(crate) fn view_window(&self) -> (f64, f64) {
        resolve_view(self.view_start, self.view_dur, self.content_duration())
    }

    /// Zoom by `steps` wheel notches — positive in — keeping whatever time sits
    /// under `anchor_x` under it afterwards.
    pub(crate) fn zoom_timeline(&mut self, steps: f64, anchor_x: f32) {
        let layout = self.timeline_layout();
        let content = self.content_duration();
        let anchor_t = layout.cursor_to_t(anchor_x);
        let along = ((anchor_x - layout.clips_x) / layout.clips_w).clamp(0.0, 1.0) as f64;
        let want = layout.view_dur / ZOOM_STEP.powf(steps);
        // Past the whole timeline the request becomes "everything" rather than
        // the length that happens to be everything right now — see
        // [`resolve_view`].
        self.view_dur = if want >= content { f64::INFINITY } else { want };
        let (_, dur) = self.view_window();
        self.view_start = anchor_t - along * dur;
    }

    /// Slide the view by `notches` of [`PAN_STEP_FRAC`], positive to the right.
    pub(crate) fn pan_timeline(&mut self, notches: f64) {
        let (start, dur) = self.view_window();
        self.view_start = start + notches * PAN_STEP_FRAC * dur;
    }

    pub(crate) fn zoom_timeline_to_fit(&mut self) {
        self.view_start = 0.0;
        self.view_dur = f64::INFINITY;
    }

    /// Keep a playhead that is moving under its own power in view.
    ///
    /// Only while playing: a scrub is the user putting the playhead somewhere
    /// themselves, and scrolling the picture out from under the hand doing it
    /// would be a fight. The new page starts a little before the playhead
    /// rather than centred on it, so the view moves once every screenful
    /// instead of continuously — and so what just played stays on screen.
    pub(crate) fn follow_playhead(&mut self, t: f64) {
        if !self.audio.playing() {
            return;
        }
        let (start, dur) = self.view_window();
        if t >= start && t < start + dur {
            return;
        }
        self.view_start = t - dur * VIEW_FOLLOW_LEAD;
    }

    /// Position of a track among the tracks of its own kind — the number in
    /// its V1/A2 label, and the argument [`lane_y`] wants.
    pub(crate) fn visual_index(&self, track_idx: usize) -> usize {
        let kind = self.timeline.tracks[track_idx].kind;
        self.timeline.tracks[..track_idx]
            .iter()
            .filter(|t| t.kind == kind)
            .count()
    }

    /// Top edge of a track's lane, in the layout `layout` describes.
    pub(crate) fn lane_top(&self, track_idx: usize, layout: &TimelineLayout) -> f32 {
        lane_y(
            layout.center_y,
            layout.lane_h,
            self.visual_index(track_idx),
            self.timeline.tracks[track_idx].kind,
        )
    }

    pub(crate) fn n_video_tracks(&self) -> usize {
        self.timeline
            .tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Video)
            .count()
    }

    /// Locate the visual track lane under `cursor_y`. Returns the track index
    /// whose lane *center* is nearest — gaps snap to the nearer lane so drops
    /// near a boundary feel forgiving.
    pub(crate) fn track_at_y(&self, cursor_y: f32, layout: &TimelineLayout) -> Option<usize> {
        let topmost = topmost_lane_top(layout.center_y, layout.lane_h, self.n_video_tracks());
        if cursor_y < topmost {
            return None;
        }
        let stride = layout.lane_h + TRACK_LANE_GAP;
        let video_idxs: Vec<usize> = self
            .timeline
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, tr)| tr.kind == TrackKind::Video)
            .map(|(i, _)| i)
            .collect();
        let audio_idxs: Vec<usize> = self
            .timeline
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, tr)| tr.kind == TrackKind::Audio)
            .map(|(i, _)| i)
            .collect();

        // Lanes start half_gap away from center_y on each side (the V/A
        // boundary's share of the inter-lane gap). Subtract it so the V1/A1
        // hitbox aligns with the rendered lane rect.
        let half_gap = TRACK_LANE_GAP * 0.5;
        if cursor_y < layout.center_y {
            let dy = (layout.center_y - cursor_y - half_gap).max(0.0);
            let visual_i = (dy / stride).floor() as usize;
            video_idxs.get(visual_i).copied()
        } else {
            let dy = (cursor_y - layout.center_y - half_gap).max(0.0);
            let visual_i = (dy / stride).floor() as usize;
            audio_idxs.get(visual_i).copied()
        }
    }

    pub(crate) fn timeline_hit(&self, cursor_x: f32, cursor_y: f32) -> TimelineHit {
        let layout = self.timeline_layout();
        if cursor_y < layout.top {
            return TimelineHit::None;
        }
        let topmost = topmost_lane_top(layout.center_y, layout.lane_h, self.n_video_tracks());
        let ruler_top = topmost - TIMELINE_RULER_H;
        if cursor_y >= ruler_top && cursor_y < topmost && cursor_x >= layout.clips_x {
            return TimelineHit::Ruler;
        }
        let Some(track_idx) = self.track_at_y(cursor_y, &layout) else {
            return TimelineHit::None;
        };
        if cursor_x < layout.clips_x {
            return TimelineHit::None;
        }
        let cursor_t = layout.cursor_to_t(cursor_x);
        let track = &self.timeline.tracks[track_idx];
        let lane_y = self.lane_top(track_idx, &layout);
        for (i, clip) in track.clips.iter().enumerate() {
            let x0 = layout.t_to_x(clip.timeline_start);
            let x1 = layout.t_to_x(clip.timeline_end());
            // Fade handles beat the trim handles they sit on top of. Each is
            // one small box in a corner, so a trim keeps the rest of the edge.
            {
                let fade_in_x = layout.t_to_x(clip.timeline_start + clip.fade_in);
                if fade_handle_rect(fade_in_x, x0, x1, lane_y).contains([cursor_x, cursor_y]) {
                    return TimelineHit::ClipFade {
                        track: track_idx,
                        idx: i,
                        side: FadeSide::In,
                    };
                }
                let fade_out_x = layout.t_to_x(clip.timeline_end() - clip.fade_out);
                if fade_handle_rect(fade_out_x, x0, x1, lane_y).contains([cursor_x, cursor_y]) {
                    return TimelineHit::ClipFade {
                        track: track_idx,
                        idx: i,
                        side: FadeSide::Out,
                    };
                }
            }
            if cursor_x >= x0 - CLIP_EDGE_GRAB_PX && cursor_x <= x0 + CLIP_EDGE_GRAB_PX {
                return TimelineHit::ClipTrimLeft { track: track_idx, idx: i };
            }
            if cursor_x >= x1 - CLIP_EDGE_GRAB_PX && cursor_x <= x1 + CLIP_EDGE_GRAB_PX {
                return TimelineHit::ClipTrimRight { track: track_idx, idx: i };
            }
            if cursor_x >= x0 && cursor_x <= x1 {
                // The level line runs the whole width of the clip, so making it
                // grabbable at rest would cost a band of the clip body on every
                // clip on the timeline. It goes live once the clip is selected
                // instead: the press that selects still moves the clip, and the
                // one after it adjusts the level.
                let level_y = level_line_y(track.kind, lane_y, layout.lane_h, clip);
                if self.selected == Some(clip.id)
                    && (cursor_y - level_y).abs() <= CLIP_LEVEL_GRAB_PX
                {
                    return TimelineHit::ClipLevel { track: track_idx, idx: i };
                }
                return TimelineHit::ClipBody {
                    track: track_idx,
                    idx: i,
                    grab_t_offset: cursor_t - clip.timeline_start,
                };
            }
        }
        TimelineHit::Lane
    }

    /// The selected clip's picture on the canvas: which clip it is, and where
    /// it was drawn in the preview panel, in points.
    ///
    /// `None` unless there is a video clip selected *and* it is live at the
    /// playhead — a clip you cannot see is one there is nothing to place.
    pub(crate) fn preview_transform_target(&self) -> Option<(usize, usize, Rect)> {
        let (track, idx) = self.selected.and_then(|id| self.timeline.find(id))?;
        if self.timeline.tracks[track].kind != TrackKind::Video {
            return None;
        }
        let clip = self.timeline.tracks[track].clips[idx];
        if !clip.contains(self.audio.position()) {
            return None;
        }
        let canvas = self.canvas();
        let (sw, sh) = self.source_size(clip.source, canvas)?;
        let [x, y, w, h] = canvas.place(sw as f32, sh as f32, clip.transform);
        let s = self.preview_canvas_scale;
        Some((
            track,
            idx,
            Rect {
                x: self.preview_canvas.x + x * s,
                y: self.preview_canvas.y + y * s,
                w: w * s,
                h: h * s,
            },
        ))
    }

    /// What the cursor is over on the preview canvas, and which clip it would
    /// act on.
    ///
    /// Corner boxes beat the body they sit inside, the same precedence the
    /// timeline's fade handles have over the trim edges under them.
    pub(crate) fn preview_hit(&self, cursor: [f32; 2]) -> Option<(usize, usize, PreviewHit)> {
        let (track, idx, rect) = self.preview_transform_target()?;
        let corner = transform_handle_rects(rect)
            .iter()
            .position(|r| r.contains(cursor))
            .map(|i| HANDLE_CORNERS[i]);
        if let Some(corner) = corner {
            return Some((track, idx, PreviewHit::Scale { corner }));
        }
        // Clipped to the canvas, not just to the picture: the part of a clip
        // hanging off the edge is not on screen, and a drag started out there
        // would be a grab on empty panel.
        if rect.contains(cursor) && self.preview_canvas.contains(cursor) {
            return Some((track, idx, PreviewHit::Move));
        }
        None
    }

    /// The cursor in canvas pixels, which is the space every placement is
    /// expressed in. `None` before the preview has been drawn at a real size.
    pub(crate) fn cursor_on_canvas(&self) -> Option<[f32; 2]> {
        // See the note in `canvas_snap_threshold`: a zero-sized window can
        // leave a NaN here, which no bound would catch on its own.
        let s = self.preview_canvas_scale;
        if !s.is_finite() || s <= 0.0 {
            return None;
        }
        Some([
            (self.cursor[0] - self.preview_canvas.x) / s,
            (self.cursor[1] - self.preview_canvas.y) / s,
        ])
    }

    pub(crate) fn pool_hit(&self, cursor_x: f32, cursor_y: f32) -> Option<SourceId> {
        let media_w = self.media_pool_w();
        let top_h = self.timeline_top();
        if cursor_x < 0.0 || cursor_x > media_w || cursor_y < POOL_LIST_TOP || cursor_y > top_h {
            return None;
        }
        let stride = POOL_ROW_HEIGHT + POOL_ROW_GAP;
        let rel_y = cursor_y - POOL_LIST_TOP;
        let i = (rel_y / stride).floor() as usize;
        let within = rel_y - i as f32 * stride;
        if within > POOL_ROW_HEIGHT {
            return None; // in the gap between rows
        }
        self.media.ids().get(i).copied()
    }

    pub(crate) fn pool_close_hit(&self, cursor_x: f32, cursor_y: f32) -> Option<SourceId> {
        let media_w = self.media_pool_w();
        let row_w = (media_w - LABEL_PAD * 2.0).max(1.0);
        for (i, &id) in self.media.ids().iter().enumerate() {
            let row_y = POOL_LIST_TOP + i as f32 * (POOL_ROW_HEIGHT + POOL_ROW_GAP);
            let close = pool_row_close_rect(LABEL_PAD, row_y, row_w);
            if close.contains([cursor_x, cursor_y]) {
                return Some(id);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LANE_Y: f32 = 100.0;
    const LANE_H: f32 = 60.0;

    #[test]
    fn a_level_line_lands_where_the_gain_it_was_read_from_puts_it() {
        for db in [MIN_GAIN_DB, -20.0, -6.0, 0.0, MAX_GAIN_DB] {
            let gain = db_to_gain(db);
            let back = y_to_gain(LANE_Y, LANE_H, gain_to_y(LANE_Y, LANE_H, gain));
            assert!(
                (gain_to_db(back) - db).abs() < 1e-3,
                "{db} dB came back as {} dB",
                gain_to_db(back)
            );
        }
    }

    /// Unity near the top and silence at the bottom, both clear of the clip's
    /// own border — the inset is what keeps the ends of the range grabbable.
    #[test]
    fn the_level_line_runs_top_to_bottom_within_the_inset() {
        let top = gain_to_y(LANE_Y, LANE_H, db_to_gain(MAX_GAIN_DB));
        let bottom = gain_to_y(LANE_Y, LANE_H, db_to_gain(MIN_GAIN_DB));
        assert_eq!(top, LANE_Y + CLIP_LEVEL_INSET);
        assert_eq!(bottom, LANE_Y + LANE_H - CLIP_LEVEL_INSET);
        assert!(gain_to_y(LANE_Y, LANE_H, 1.0) < LANE_Y + LANE_H * 0.5);
    }

    #[test]
    fn a_drag_off_the_end_of_the_lane_pins_to_the_end_of_the_range() {
        assert_eq!(y_to_gain(LANE_Y, LANE_H, 0.0), db_to_gain(MAX_GAIN_DB));
        assert_eq!(y_to_gain(LANE_Y, LANE_H, 10_000.0), db_to_gain(MIN_GAIN_DB));
    }

    /// A fade of zero would otherwise centre its handle on the clip's edge and
    /// hang half of it over the neighbour, where the neighbour's own handle is.
    #[test]
    fn a_fade_handle_stays_inside_the_clip_it_belongs_to() {
        let (x0, x1) = (200.0, 300.0);
        let head = fade_handle_rect(x0, x0, x1, LANE_Y);
        assert_eq!(head.x, x0);
        let tail = fade_handle_rect(x1, x0, x1, LANE_Y);
        assert_eq!(tail.x + tail.w, x1);
    }

    /// A clip narrower than one handle still gets one, pinned to its start,
    /// rather than a rect that starts to the right of where it ends.
    #[test]
    fn a_clip_narrower_than_a_handle_still_places_one() {
        let (x0, x1) = (200.0, 204.0);
        let r = fade_handle_rect(x1, x0, x1, LANE_Y);
        assert_eq!(r.x, x0);
    }

    fn view(start: f64, dur: f64) -> TimelineLayout {
        TimelineLayout {
            top: 0.0,
            clips_x: 48.0,
            clips_w: 1000.0,
            center_y: 400.0,
            lane_h: LANE_H,
            bottom: 800.0,
            view_start: start,
            view_dur: dur,
            content: 100.0,
        }
    }

    #[test]
    fn a_time_maps_to_the_x_it_reads_back_from() {
        let l = view(30.0, 20.0);
        for t in [30.0, 35.0, 42.5, 50.0] {
            let back = l.cursor_to_t(l.t_to_x(t));
            assert!((back - t).abs() < 1e-6, "{t} came back as {back}");
        }
        // The window's ends are the panel's ends.
        assert_eq!(l.t_to_x(30.0), l.clips_x);
        assert_eq!(l.t_to_x(50.0), l.clips_right());
    }

    #[test]
    fn a_cursor_off_the_panel_reads_as_the_edge_it_ran_off() {
        let l = view(30.0, 20.0);
        assert_eq!(l.cursor_to_t(0.0), 30.0);
        assert_eq!(l.cursor_to_t(9_999.0), 50.0);
    }

    #[test]
    fn zoom_is_what_the_pixel_threshold_converts_through() {
        assert_eq!(view(0.0, 100.0).px_per_sec(), 10.0);
        assert_eq!(view(0.0, 10.0).px_per_sec(), 100.0);
    }

    /// The default `view_dur` is infinity, and what makes it mean "fit" is
    /// that it resolves to whatever content there is at the time.
    #[test]
    fn an_unzoomed_view_spans_the_content_however_long_it_grows() {
        assert_eq!(resolve_view(0.0, f64::INFINITY, 12.0), (0.0, 12.0));
        assert_eq!(resolve_view(0.0, f64::INFINITY, 3600.0), (0.0, 3600.0));
        // And an empty timeline still has a finite mapping to divide by.
        assert_eq!(resolve_view(0.0, f64::INFINITY, 0.0), (0.0, MIN_CONTENT_SPAN));
    }

    #[test]
    fn a_view_cannot_be_zoomed_or_scrolled_off_the_content() {
        assert_eq!(resolve_view(0.0, 1e-9, 60.0).1, MIN_VIEW_SPAN);
        assert_eq!(resolve_view(0.0, 600.0, 60.0), (0.0, 60.0));
        // Scrolled past the end, the last window of content is what's left.
        assert_eq!(resolve_view(1000.0, 10.0, 60.0), (50.0, 10.0));
        assert_eq!(resolve_view(-5.0, 10.0, 60.0), (0.0, 10.0));
    }

    #[test]
    fn a_view_that_shows_everything_has_no_thumb_to_drag() {
        assert!(view(0.0, 100.0).scroll_thumb().is_none());
        assert!(view(0.0, 40.0).scroll_thumb().is_some());
    }

    #[test]
    fn the_thumb_measures_the_view_against_the_whole_timeline() {
        let l = view(0.0, 10.0);
        let (track, thumb) = (l.scrollbar_rect(), l.scroll_thumb().unwrap());
        // A tenth of the timeline is a tenth of the strip, parked at the start.
        assert_eq!(thumb.w, track.w * 0.1);
        assert_eq!(thumb.x, track.x);

        // And at the end of the timeline it ends where the strip does, rather
        // than running past it.
        let end = view(90.0, 10.0).scroll_thumb().unwrap();
        assert_eq!(end.x + end.w, track.x + track.w);
    }

    #[test]
    fn dragging_the_thumb_reads_back_the_view_it_was_drawn_for() {
        for start in [0.0, 12.5, 37.0, 90.0] {
            let l = view(start, 10.0);
            let thumb = l.scroll_thumb().unwrap();
            let back = l.scroll_x_to_view_start(thumb.x);
            assert!((back - start).abs() < 1e-6, "{start} came back as {back}");
        }
    }

    /// Zoomed far enough in the thumb would be a fraction of a point wide, so
    /// it stops shrinking — and its travel has to take up the difference, or
    /// the far end of the strip would no longer mean the end of the timeline.
    #[test]
    fn a_thumb_pinned_to_its_minimum_still_reaches_both_ends() {
        let l = view(0.0, MIN_VIEW_SPAN);
        let (track, thumb) = (l.scrollbar_rect(), l.scroll_thumb().unwrap());
        assert_eq!(thumb.w, SCROLLBAR_MIN_THUMB_W);
        assert_eq!(l.scroll_x_to_view_start(track.x), 0.0);
        let far = l.scroll_x_to_view_start(track.x + track.w - thumb.w);
        assert!((far - (100.0 - MIN_VIEW_SPAN)).abs() < 1e-9, "{far}");
    }

    #[test]
    fn lanes_stack_away_from_the_divider_they_share() {
        let center = 500.0;
        let h = 40.0;
        let v1 = lane_y(center, h, 0, TrackKind::Video);
        let v2 = lane_y(center, h, 1, TrackKind::Video);
        let a1 = lane_y(center, h, 0, TrackKind::Audio);
        assert!(v1 + h < center && a1 > center);
        assert_eq!(v2 + h + TRACK_LANE_GAP, v1);
        // The gap at the divider is shared, half to each side.
        assert_eq!(center - (v1 + h), a1 - center);
    }
}
