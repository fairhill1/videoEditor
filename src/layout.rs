//! Where things are, and what the cursor is over.
//!
//! Everything here is geometry: resolving the panel splits, mapping between
//! time and x, finding the lane under a y, and hit-testing the media pool.
//! Drawing lives elsewhere and asks these questions rather than answering them
//! again, which is what keeps a rect you can click on and the rect you can see
//! from drifting apart.

use crate::state::State;
use crate::theme::*;
use crate::timeline::{SourceId, TrackKind};
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
    Lane { track: usize },
    ClipBody { track: usize, idx: usize, grab_t_offset: f64 },
    ClipTrimLeft { track: usize, idx: usize },
    ClipTrimRight { track: usize, idx: usize },
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

/// Top edge of the highest video lane. Video stacks upward from the V/A
/// divider, so this is where the ruler has to end.
pub(crate) fn topmost_lane_top(center_y: f32, lane_h: f32, n_video: usize) -> f32 {
    let half_gap = TRACK_LANE_GAP * 0.5;
    if n_video == 0 {
        center_y
    } else {
        center_y - half_gap - lane_h * n_video as f32 - (n_video as f32 - 1.0) * TRACK_LANE_GAP
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
        for (i, clip) in track.clips.iter().enumerate() {
            let x0 = layout.t_to_x(clip.timeline_start);
            let x1 = layout.t_to_x(clip.timeline_end());
            if cursor_x >= x0 - CLIP_EDGE_GRAB_PX && cursor_x <= x0 + CLIP_EDGE_GRAB_PX {
                return TimelineHit::ClipTrimLeft { track: track_idx, idx: i };
            }
            if cursor_x >= x1 - CLIP_EDGE_GRAB_PX && cursor_x <= x1 + CLIP_EDGE_GRAB_PX {
                return TimelineHit::ClipTrimRight { track: track_idx, idx: i };
            }
            if cursor_x >= x0 && cursor_x <= x1 {
                return TimelineHit::ClipBody {
                    track: track_idx,
                    idx: i,
                    grab_t_offset: cursor_t - clip.timeline_start,
                };
            }
        }
        TimelineHit::Lane { track: track_idx }
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
