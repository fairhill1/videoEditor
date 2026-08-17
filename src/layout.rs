//! Where things are, and what the cursor is over.
//!
//! Everything here is geometry: resolving the panel splits, mapping between
//! time and x, finding the lane under a y, and hit-testing the media pool.
//! Drawing lives elsewhere and asks these questions rather than answering them
//! again, which is what keeps a rect you can click on and the rect you can see
//! from drifting apart.

use crate::state::State;
use crate::theme::*;
use crate::timeline::{db_to_gain, gain_to_db, FadeSide, SourceId, TrackKind, MAX_GAIN_DB, MIN_GAIN_DB};
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
    pub(crate) duration: f64,
}

impl TimelineLayout {
    pub(crate) fn cursor_to_t(&self, cursor_x: f32) -> f64 {
        let ratio = ((cursor_x - self.clips_x) / self.clips_w).clamp(0.0, 1.0) as f64;
        ratio * self.duration
    }

    pub(crate) fn t_to_x(&self, t: f64) -> f32 {
        self.clips_x + (t / self.duration) as f32 * self.clips_w
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
        let tracks_area_h = (bottom - tracks_top).max(0.0);
        TimelineLayout {
            top,
            clips_x: TRACK_HEADER_WIDTH,
            clips_w: (w - TRACK_HEADER_WIDTH).max(1.0),
            center_y: (tracks_top + tracks_area_h * 0.5).round(),
            lane_h: compute_lane_height(tracks_area_h, self.timeline.tracks.len()),
            duration: self.timeline.duration().max(1.0),
        }
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
        let has_level = track.kind == TrackKind::Audio;
        for (i, clip) in track.clips.iter().enumerate() {
            let x0 = layout.t_to_x(clip.timeline_start);
            let x1 = layout.t_to_x(clip.timeline_end());
            // Fade handles beat the trim handles they sit on top of. Each is
            // one small box in a corner, so a trim keeps the rest of the edge.
            if has_level {
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
                let level_y = gain_to_y(lane_y, layout.lane_h, clip.gain);
                if has_level
                    && self.selected == Some(clip.id)
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
