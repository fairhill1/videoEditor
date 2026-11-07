//! Where things are, and what the cursor is over.
//!
//! Everything here is geometry: resolving the panel splits and the timeline's
//! visible window, mapping between time and x, finding the lane under a y, and
//! hit-testing the media pool. Drawing lives elsewhere and asks these questions
//! rather than answering them again, which is what keeps a rect you can click
//! on and the rect you can see from drifting apart.

use crate::effects::{EffectKind, EffectStack};
use crate::fonts;
use crate::input::DragMode;
use crate::state::State;
use crate::theme::*;
use crate::timeline::{
    db_to_gain, gain_to_db, Clip, FadeSide, SourceId, TrackKind, MAX_GAIN_DB, MAX_OPACITY,
    MIN_GAIN_DB, MIN_OPACITY,
};
use crate::title::{Title, TitleColor, TitleNumber, TitlePart, SWATCHES};
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

/// Where a view `dur` long sits on the zoom slider: 0 shows all of `content`,
/// 1 is zoomed in as far as [`MIN_VIEW_SPAN`].
///
/// Logarithmic, so equal distances along the slider are equal zoom ratios, as
/// wheel notches are. A linear slider would spend nearly its whole length on
/// the widest views and cram every useful close-up into its last few points.
pub(crate) fn zoom_fraction(dur: f64, content: f64) -> f64 {
    let range = (content / MIN_VIEW_SPAN).ln();
    if range <= 0.0 {
        return 0.0;
    }
    ((content / dur).ln() / range).clamp(0.0, 1.0)
}

/// The view length at `fraction` along the zoom slider; the inverse of
/// [`zoom_fraction`].
pub(crate) fn zoom_span(fraction: f64, content: f64) -> f64 {
    content / (content / MIN_VIEW_SPAN).max(1.0).powf(fraction.clamp(0.0, 1.0))
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
    /// The fx badge on a clip that has effects.
    ClipEffects { track: usize, idx: usize },
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

/// What the timeline's lanes share their panel with: the ruler, and the add
/// row at each end of the stack.
const TRACKS_CHROME_H: f32 = TIMELINE_RULER_H + TRACK_ADD_ROW_H * 2.0;

/// The line between video and audio, for `n` tracks of which `n_video` are
/// video, in a panel whose lanes start at `tracks_top` and get `area_h`.
///
/// The whole block — ruler, add rows, lanes — is centred, with video stacking
/// up from this line and audio down from it. The line sits wherever the video
/// lanes end, so a timeline with more video tracks than audio stays centred
/// rather than running up into the toolbar. Never above the panel's top: a
/// block taller than the panel runs off the bottom instead, away from the
/// toolbar. Whole points, so the lane edges derived from it don't land on
/// halves, which render blurred.
pub(crate) fn lanes_center_y(tracks_top: f32, area_h: f32, lane_h: f32, n_video: usize, n: usize) -> f32 {
    let lanes_h = n as f32 * lane_h + n.saturating_sub(1) as f32 * TRACK_LANE_GAP;
    let block_top = (tracks_top + (area_h - TRACKS_CHROME_H - lanes_h) * 0.5).max(tracks_top);
    (block_top + TIMELINE_RULER_H + TRACK_ADD_ROW_H + n_video as f32 * (lane_h + TRACK_LANE_GAP)
        - TRACK_LANE_GAP * 0.5)
        .round()
}

/// The add rows for a stack of lanes about `center_y`: one above the top
/// video lane, one below the bottom audio lane — where a new track appears.
pub(crate) fn track_add_rows(
    center_y: f32,
    lane_h: f32,
    n_video: usize,
    n_audio: usize,
    w: f32,
) -> [(TrackKind, Rect); 2] {
    let topmost = topmost_lane_top(center_y, lane_h, n_video);
    let bottom = if n_audio == 0 {
        center_y
    } else {
        lane_y(center_y, lane_h, n_audio - 1, TrackKind::Audio) + lane_h
    };
    [
        (TrackKind::Video, Rect { x: 0.0, y: topmost - TRACK_ADD_ROW_H, w, h: TRACK_ADD_ROW_H }),
        (TrackKind::Audio, Rect { x: 0.0, y: bottom, w, h: TRACK_ADD_ROW_H }),
    ]
}

pub(crate) fn compute_lane_height(tracks_area_h: f32, n_tracks: usize) -> f32 {
    let n = n_tracks.max(1) as f32;
    let gaps = (n - 1.0).max(0.0) * TRACK_LANE_GAP;
    let avail = (tracks_area_h * TRACK_LANE_FILL - gaps).max(0.0);
    (avail / n)
        .clamp(TRACK_LANE_MIN_H, TRACK_LANE_MAX_H)
        .round()
}

/// How the media pool lays its sources out.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub(crate) enum PoolView {
    /// One row per source: a small thumbnail with the name and format beside it.
    #[default]
    List,
    /// Tiles: a large thumbnail with the name under it.
    Grid,
}

/// Where one pool entry sits: the whole cell, which a press picks up and the
/// close button sits in the corner of, and the thumbnail slot inside it, which
/// a hover scrubs.
#[derive(Copy, Clone, Debug)]
pub(crate) struct PoolCell {
    pub(crate) cell: Rect,
    pub(crate) thumb: Rect,
}

/// The cells for the first `n` pool entries in a pool `pool_w` wide, stopping
/// at the first one that would run past `pool_h`.
///
/// The one place pool geometry is worked out: drawing and every pool hit test
/// read it from here, so a view switch cannot leave a row clickable where it
/// used to be drawn.
pub(crate) fn pool_cells(view: PoolView, pool_w: f32, pool_h: f32, n: usize) -> Vec<PoolCell> {
    let avail = (pool_w - LABEL_PAD * 2.0).max(1.0);
    let mut cells = Vec::with_capacity(n);
    match view {
        PoolView::List => {
            for i in 0..n {
                let y = POOL_LIST_TOP + i as f32 * (POOL_ROW_HEIGHT + POOL_ROW_GAP);
                if y + POOL_ROW_HEIGHT > pool_h {
                    break;
                }
                cells.push(PoolCell {
                    cell: Rect { x: LABEL_PAD, y, w: avail, h: POOL_ROW_HEIGHT },
                    thumb: Rect {
                        x: LABEL_PAD + POOL_ROW_PAD,
                        y: y + POOL_ROW_PAD,
                        w: POOL_THUMB_W,
                        h: POOL_THUMB_H,
                    },
                });
            }
        }
        PoolView::Grid => {
            // As many columns as fit at the minimum tile width, then the tiles
            // widen to share out what is left, so the grid always spans the
            // pool rather than leaving a ragged strip down the right.
            let cols = ((avail + POOL_TILE_GAP) / (POOL_TILE_MIN_W + POOL_TILE_GAP))
                .floor()
                .max(1.0);
            let tile_w = ((avail - POOL_TILE_GAP * (cols - 1.0)) / cols).floor();
            let thumb_w = tile_w - POOL_ROW_PAD * 2.0;
            let thumb_h = (thumb_w * 9.0 / 16.0).round();
            let tile_h = POOL_ROW_PAD + thumb_h + POOL_TILE_NAME_H;
            let cols = cols as usize;
            for i in 0..n {
                let (col, row) = ((i % cols) as f32, (i / cols) as f32);
                let x = LABEL_PAD + col * (tile_w + POOL_TILE_GAP);
                let y = POOL_LIST_TOP + row * (tile_h + POOL_TILE_GAP);
                if y + tile_h > pool_h {
                    break;
                }
                cells.push(PoolCell {
                    cell: Rect { x, y, w: tile_w, h: tile_h },
                    thumb: Rect {
                        x: x + POOL_ROW_PAD,
                        y: y + POOL_ROW_PAD,
                        w: thumb_w,
                        h: thumb_h,
                    },
                });
            }
        }
    }
    cells
}

/// Which page the pool panel shows: the media in the project, or the effects
/// that can be put on it.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub(crate) enum PoolTab {
    #[default]
    Media,
    Effects,
}

/// One card in the effects library: the tile a drag picks up, the picture of
/// what the effect does, and the apply button in the picture's corner.
#[derive(Copy, Clone, Debug)]
pub(crate) struct LibraryCard {
    pub(crate) kind: EffectKind,
    pub(crate) card: PoolCell,
    pub(crate) apply: Rect,
}

/// One parameter of an applied effect: its row, and the slider in it.
#[derive(Copy, Clone, Debug)]
pub(crate) struct ParamRow {
    pub(crate) param: usize,
    pub(crate) row: Rect,
    pub(crate) slider: Rect,
}

/// One effect applied to the clip the panel is showing: its own row, with the
/// on-off and remove buttons, and its parameters under it.
#[derive(Clone, Debug)]
pub(crate) struct StackRow {
    pub(crate) index: usize,
    pub(crate) row: Rect,
    pub(crate) toggle: Rect,
    pub(crate) remove: Rect,
    pub(crate) params: Vec<ParamRow>,
}

/// One control on a title: what it changes. The panel shows one row per
/// control when the clip it is showing is a title.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) enum TitleControl {
    /// The words, in a box that starts typing when pressed.
    Text,
    /// The face, in a box that opens the list of installed fonts.
    Font,
    Number(TitleNumber),
    Color(TitleColor),
    /// The on-off switch heading the outline's or shadow's rows.
    Toggle(TitlePart),
}

/// One row of a title's settings: the row, and the control in it — the text
/// box, the font box, the slider, the swatch strip, or the switch.
#[derive(Copy, Clone, Debug)]
pub(crate) struct TitleRow {
    pub(crate) control: TitleControl,
    pub(crate) row: Rect,
    pub(crate) field: Rect,
}

/// Where everything on the effects tab sits. Like [`pool_cells`], the one
/// place this is worked out, for drawing and hit testing alike.
///
/// Every rect is already scrolled: it is where the thing is on screen, and
/// may be partly or wholly outside the panel's visible band, which drawing
/// clips to and [`EffectsPanel::hit`] refuses presses outside of.
#[derive(Clone, Debug)]
pub(crate) struct EffectsPanel {
    pub(crate) library: Vec<LibraryCard>,
    /// Top of the heading over the selected clip's settings.
    pub(crate) clip_heading_y: f32,
    /// The title's settings, when the clip is a title, and the blocks they
    /// are grouped into: the words and their look, the outline, the shadow.
    pub(crate) title: Vec<TitleRow>,
    pub(crate) title_groups: Vec<Rect>,
    /// Top of the line that stands in for the stack when it has nothing to
    /// list — no clip picked, or a clip with no effects yet.
    pub(crate) hint_y: f32,
    pub(crate) stack: Vec<StackRow>,
    /// The band the panel shows, top to bottom; content above or below it is
    /// scrolled out of sight.
    pub(crate) visible: [f32; 2],
    /// How far the content can scroll: its height past the visible band.
    pub(crate) max_scroll: f32,
}

/// What a press on the effects tab landed on.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) enum EffectsHit {
    /// A library card away from its button: the start of a drag to a clip.
    Library(EffectKind),
    Apply(EffectKind),
    Toggle { index: usize },
    Remove { index: usize },
    Param { index: usize, param: usize, slider: Rect },
    TitleText,
    TitleFont,
    TitleNumber { field: TitleNumber, slider: Rect },
    TitleSwatch { field: TitleColor, swatch: usize },
    TitleToggle(TitlePart),
}

/// How tall the text box is for `lines` lines: every one of them, blank ones
/// too, up to a cap past which it shows the end.
pub(crate) fn text_box_h(lines: usize) -> f32 {
    let shown = lines.clamp(1, TITLE_TEXT_MAX_LINES) as f32;
    (shown * TITLE_TEXT_LINE_H + (TITLE_FIELD_H - TITLE_TEXT_LINE_H)).round()
}

/// The swatches in a colour row's strip, one per entry in [`SWATCHES`]:
/// square, in rows of [`TITLE_SWATCH_COLUMNS`], spaced evenly and centred
/// down the strip.
pub(crate) fn swatch_rects(field: Rect) -> [Rect; SWATCHES.len()] {
    let cols = TITLE_SWATCH_COLUMNS as f32;
    let rows = SWATCHES.len().div_ceil(TITLE_SWATCH_COLUMNS) as f32;
    let side = TITLE_SWATCH
        .min(((field.w - TITLE_SWATCH_GAP * (cols - 1.0)) / cols).floor())
        .max(4.0);
    let block_h = rows * side + (rows - 1.0) * TITLE_SWATCH_GAP;
    let top = (field.y + (field.h - block_h) * 0.5).round();
    std::array::from_fn(|i| Rect {
        x: field.x + (i % TITLE_SWATCH_COLUMNS) as f32 * (side + TITLE_SWATCH_GAP),
        y: top + (i / TITLE_SWATCH_COLUMNS) as f32 * (side + TITLE_SWATCH_GAP),
        w: side,
        h: side,
    })
}

/// A colour row: tall enough for every row of swatches, with the same margin
/// above and below as a slider row has around its knob.
fn swatch_row_h() -> f32 {
    let rows = SWATCHES.len().div_ceil(TITLE_SWATCH_COLUMNS) as f32;
    rows * TITLE_SWATCH + (rows - 1.0) * TITLE_SWATCH_GAP + (EFFECT_PARAM_H - TITLE_SWATCH)
}

/// Lay the effects tab out in a pool `pool_w` wide and `pool_h` tall: the
/// library as cards, then the clip being shown — its title settings when
/// `title_lines` says it is one (and how many lines its text box needs),
/// then its effects, `stack` — scrolled up by `scroll`.
///
/// The cards are the media pool's grid tiles exactly, so the two pages of the
/// panel read as one kind of thing. The library comes first so it holds
/// still: it is what you drag from, and cards that moved down whenever the
/// selected clip gained an effect would move out from under the cursor
/// between one drag and the next.
pub(crate) fn effects_panel(
    pool_w: f32,
    pool_h: f32,
    kinds: &[EffectKind],
    title_lines: Option<usize>,
    stack: Option<&EffectStack>,
    scroll: f32,
) -> EffectsPanel {
    let x = LABEL_PAD;
    let w = (pool_w - LABEL_PAD * 2.0).max(1.0);
    // Laid out unscrolled, top at `POOL_LIST_TOP`, and moved up at the end.
    let cells = pool_cells(PoolView::Grid, pool_w, f32::INFINITY, kinds.len());
    let mut y = cells.iter().map(|c| c.cell.y + c.cell.h).fold(POOL_LIST_TOP, f32::max);
    let mut library: Vec<LibraryCard> = kinds
        .iter()
        .zip(cells)
        .map(|(&kind, card)| LibraryCard {
            kind,
            card,
            apply: Rect {
                x: card.thumb.x + card.thumb.w - POOL_CLOSE_INSET - EFFECT_SMALL_BTN,
                y: card.thumb.y + POOL_CLOSE_INSET,
                w: EFFECT_SMALL_BTN,
                h: EFFECT_SMALL_BTN,
            },
        })
        .collect();

    y += EFFECTS_SECTION_GAP;
    let clip_heading_y = y;
    y += EFFECTS_SECTION_H;

    // Where a labelled control starts: past the label, as an effect's
    // sliders do, so every control in the panel lines up.
    let field_x = x + EFFECT_PARAM_INDENT + EFFECT_PARAM_LABEL_W;
    let field_right = x + w - POOL_ROW_PAD;
    let slider_in = |row: Rect| Rect {
        x: field_x,
        y: (row.y + (row.h - ZOOM_SLIDER_KNOB_H) * 0.5).round(),
        w: (field_right - EFFECT_PARAM_VALUE_W - field_x).max(ZOOM_SLIDER_KNOB_W),
        h: ZOOM_SLIDER_KNOB_H,
    };
    let toggle_in = |row: Rect| Rect {
        x: x + POOL_ROW_PAD,
        y: (row.y + (row.h - EFFECT_SMALL_BTN) * 0.5).round(),
        w: EFFECT_SMALL_BTN,
        h: EFFECT_SMALL_BTN,
    };

    let mut title_rows = Vec::new();
    let mut title_groups = Vec::new();
    if let Some(lines) = title_lines {
        use TitleControl as C;
        let text_h = text_box_h(lines);
        let groups: [&[TitleControl]; 3] = [
            &[C::Text, C::Font, C::Number(TitleNumber::Size), C::Color(TitleColor::Fill)],
            &[
                C::Toggle(TitlePart::Outline),
                C::Number(TitleNumber::OutlineWidth),
                C::Color(TitleColor::Outline),
            ],
            &[
                C::Toggle(TitlePart::Shadow),
                C::Number(TitleNumber::ShadowDistance),
                C::Number(TitleNumber::ShadowSoftness),
                C::Number(TitleNumber::ShadowOpacity),
            ],
        ];
        for group in groups {
            let top = y;
            for &control in group {
                let h = match control {
                    C::Text => text_h + (EFFECT_ROW_H - TITLE_FIELD_H),
                    C::Toggle(_) => EFFECT_ROW_H,
                    C::Color(_) => swatch_row_h(),
                    _ => EFFECT_PARAM_H,
                };
                let row = Rect { x, y, w, h };
                let field_h = if control == C::Text { text_h } else { TITLE_FIELD_H };
                let boxed = Rect {
                    x: field_x,
                    y: (y + (h - field_h) * 0.5).round(),
                    w: (field_right - field_x).max(1.0),
                    h: field_h,
                };
                let field = match control {
                    C::Text | C::Font => boxed,
                    C::Number(_) => slider_in(row),
                    C::Color(_) => Rect { y: row.y, h: row.h, ..boxed },
                    C::Toggle(_) => toggle_in(row),
                };
                title_rows.push(TitleRow { control, row, field });
                y += h;
            }
            title_groups.push(Rect { x, y: top, w, h: y - top });
            y += EFFECT_ROW_GAP;
        }
        y += EFFECTS_SECTION_GAP - EFFECT_ROW_GAP;
    }
    let hint_y = y;

    let mut rows = Vec::new();
    for (index, effect) in stack.into_iter().flat_map(|s| s.iter()).enumerate() {
        let row = Rect { x, y, w, h: EFFECT_ROW_H };
        let toggle = toggle_in(row);
        let remove = Rect { x: x + w - POOL_ROW_PAD - EFFECT_SMALL_BTN, ..toggle };
        y += EFFECT_ROW_H;
        let mut params = Vec::new();
        for param in 0..effect.kind().params().len() {
            let prow = Rect { x, y, w, h: EFFECT_PARAM_H };
            params.push(ParamRow { param, row: prow, slider: slider_in(prow) });
            y += EFFECT_PARAM_H;
        }
        rows.push(StackRow { index, row, toggle, remove, params });
        y += EFFECT_ROW_GAP;
    }
    // Room for the hint line under an empty stack, so scrolling to the end
    // still shows it.
    let bottom = if rows.is_empty() { hint_y + EFFECT_ROW_H } else { y };

    let max_scroll = (bottom + LABEL_PAD - pool_h).max(0.0);
    let scroll = scroll.clamp(0.0, max_scroll);
    let up = |r: Rect| Rect { y: r.y - scroll, ..r };
    for lib in &mut library {
        lib.card.cell = up(lib.card.cell);
        lib.card.thumb = up(lib.card.thumb);
        lib.apply = up(lib.apply);
    }
    for r in &mut title_rows {
        r.row = up(r.row);
        r.field = up(r.field);
    }
    for g in &mut title_groups {
        *g = up(*g);
    }
    for r in &mut rows {
        r.row = up(r.row);
        r.toggle = up(r.toggle);
        r.remove = up(r.remove);
        for p in &mut r.params {
            p.row = up(p.row);
            p.slider = up(p.slider);
        }
    }

    EffectsPanel {
        library,
        clip_heading_y: clip_heading_y - scroll,
        title: title_rows,
        title_groups,
        hint_y: hint_y - scroll,
        stack: rows,
        visible: [POOL_LIST_TOP, pool_h],
        max_scroll,
    }
}

impl EffectsPanel {
    /// What is under `p`. Buttons before the rows they sit in, and nothing
    /// for a point in the part of the content scrolled out of sight.
    pub(crate) fn hit(&self, p: [f32; 2]) -> Option<EffectsHit> {
        if p[1] < self.visible[0] || p[1] >= self.visible[1] {
            return None;
        }
        for lib in &self.library {
            if lib.apply.contains(p) {
                return Some(EffectsHit::Apply(lib.kind));
            }
            if lib.card.cell.contains(p) {
                return Some(EffectsHit::Library(lib.kind));
            }
        }
        for r in &self.title {
            match r.control {
                TitleControl::Text if r.field.contains(p) => return Some(EffectsHit::TitleText),
                TitleControl::Font if r.field.contains(p) => return Some(EffectsHit::TitleFont),
                // The whole row past the label, as for an effect's slider.
                TitleControl::Number(field)
                    if r.row.contains(p) && p[0] >= r.field.x - ZOOM_SLIDER_KNOB_W =>
                {
                    return Some(EffectsHit::TitleNumber { field, slider: r.field });
                }
                TitleControl::Color(field) => {
                    if let Some(swatch) = swatch_rects(r.field).iter().position(|s| s.contains(p)) {
                        return Some(EffectsHit::TitleSwatch { field, swatch });
                    }
                }
                // The switch, or the name beside it: the row is the switch's
                // label, and a label that does nothing when clicked is a trap.
                TitleControl::Toggle(part) if r.row.contains(p) => {
                    return Some(EffectsHit::TitleToggle(part));
                }
                _ => {}
            }
        }
        for row in &self.stack {
            if row.toggle.contains(p) {
                return Some(EffectsHit::Toggle { index: row.index });
            }
            if row.remove.contains(p) {
                return Some(EffectsHit::Remove { index: row.index });
            }
            for pr in &row.params {
                // The whole row grabs, not just the knob's height: a slider
                // is a thin target, and the row around it means nothing else.
                if pr.row.contains(p) && p[0] >= pr.slider.x - ZOOM_SLIDER_KNOB_W {
                    return Some(EffectsHit::Param {
                        index: row.index,
                        param: pr.param,
                        slider: pr.slider,
                    });
                }
            }
        }
        None
    }

    /// The font box, for the list that drops from it.
    pub(crate) fn font_field(&self) -> Option<Rect> {
        self.title.iter().find(|r| r.control == TitleControl::Font).map(|r| r.field)
    }
}

/// The list of fonts dropped from the font box `anchor`: where it sits, and
/// the rows in it, which are `n` long and scrolled down by `scroll` rows.
/// Below the box when it fits and above when it does not, inside a window
/// `win` big.
#[derive(Clone, Debug)]
pub(crate) struct FontMenu {
    pub(crate) rect: Rect,
    /// Each visible row with the index of the entry it shows.
    pub(crate) rows: Vec<(usize, Rect)>,
    pub(crate) max_scroll: usize,
}

pub(crate) fn font_menu(anchor: Rect, win: [f32; 2], n: usize, scroll: usize) -> FontMenu {
    let stride = MENU_ROW_H + MENU_ROW_GAP;
    let below = anchor.y + anchor.h + MENU_GAP;
    let room_below = win[1] - MENU_PAD - below;
    let room_above = anchor.y - MENU_GAP - MENU_PAD;
    let fit = |room: f32| ((room - MENU_PAD * 2.0) / stride).floor().max(0.0) as usize;
    let rows_shown = n.min(FONT_MENU_ROWS);
    let (shown, y) = if fit(room_below) >= rows_shown || room_below >= room_above {
        let shown = rows_shown.min(fit(room_below)).max(1);
        (shown, below)
    } else {
        let shown = rows_shown.min(fit(room_above)).max(1);
        (shown, anchor.y - MENU_GAP - (shown as f32 * stride + MENU_PAD * 2.0))
    };
    let w = anchor.w.max(FONT_MENU_MIN_W);
    let x = anchor.x.min(win[0] - w - MENU_PAD).max(MENU_PAD);
    let h = shown as f32 * stride + MENU_PAD * 2.0 - MENU_ROW_GAP;
    let rect = Rect { x: x.round(), y: y.round(), w, h };
    let max_scroll = n.saturating_sub(shown);
    let scroll = scroll.min(max_scroll);
    let rows = (0..shown.min(n))
        .map(|i| {
            let r = Rect {
                x: rect.x + MENU_PAD,
                y: rect.y + MENU_PAD + i as f32 * stride,
                w: w - MENU_PAD * 2.0,
                h: MENU_ROW_H,
            };
            (scroll + i, r)
        })
        .collect();
    FontMenu { rect, rows, max_scroll }
}

/// The fx badge on a clip with effects, given the left of the clip's visible
/// part and its right edge, or `None` on a clip too narrow to hold it beside
/// its trim handles.
pub(crate) fn fx_badge_rect(vis_x0: f32, x1: f32, lane_y: f32) -> Option<Rect> {
    let x = vis_x0 + FX_BADGE_INSET_X;
    if x + FX_BADGE_W > x1 - CLIP_EDGE_GRAB_PX {
        return None;
    }
    Some(Rect { x, y: lane_y + FX_BADGE_INSET_Y, w: FX_BADGE_W, h: FX_BADGE_H })
}

/// A button in a track header, right-aligned and centred on a row starting
/// at `y` and `h` tall: the + in an add row, the remove on an empty lane.
pub(crate) fn track_header_button(y: f32, h: f32) -> Rect {
    Rect {
        x: TRACK_HEADER_WIDTH - 6.0 - TRACK_HEADER_BTN,
        y: (y + (h - TRACK_HEADER_BTN) * 0.5).round(),
        w: TRACK_HEADER_BTN,
        h: TRACK_HEADER_BTN,
    }
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
        // The ruler and the two add-track rows take their height first; the
        // lanes share out what is left.
        let n = self.timeline.tracks.len();
        let lane_h = compute_lane_height((tracks_area_h - TRACKS_CHROME_H).max(0.0), n);
        let center_y = lanes_center_y(tracks_top, tracks_area_h, lane_h, self.n_video_tracks(), n);
        TimelineLayout {
            top,
            bottom,
            clips_x: TRACK_HEADER_WIDTH,
            clips_w: (w - TRACK_HEADER_WIDTH).max(1.0),
            // Snap the centre to a whole point so the lane edges derived from
            // it don't land on halves, which render blurred.
            center_y,
            lane_h,
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
            DragMode::PoolDrag { source } => self.media.drop_duration(source),
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

    /// Zoom to `fraction` along the zoom slider, about the playhead as the
    /// keyboard zoom is: the slider is in the toolbar, and where the pointer
    /// sits on it says nothing about which part of the edit to look at.
    pub(crate) fn zoom_timeline_to_fraction(&mut self, fraction: f64) {
        if fraction <= 0.0 {
            // Exactly the fit state, rather than a span that happens to equal
            // today's content and would stop fitting once a clip is added.
            self.zoom_timeline_to_fit();
            return;
        }
        let (_, dur) = self.view_window();
        let want = zoom_span(fraction, self.content_duration());
        self.zoom_at_playhead((dur / want).ln() / ZOOM_STEP.ln());
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

    pub(crate) fn n_tracks_of(&self, kind: TrackKind) -> usize {
        self.timeline.tracks.iter().filter(|t| t.kind == kind).count()
    }

    /// The rows a new track is made in: above the top video lane, and below
    /// the bottom audio lane — where the new track will appear.
    pub(crate) fn track_add_rows(&self, layout: &TimelineLayout) -> [(TrackKind, Rect); 2] {
        track_add_rows(
            layout.center_y,
            layout.lane_h,
            self.n_video_tracks(),
            self.n_tracks_of(TrackKind::Audio),
            layout.clips_right(),
        )
    }

    /// Top of the ruler, which sits on top of the video add row.
    pub(crate) fn ruler_top(&self, layout: &TimelineLayout) -> f32 {
        self.track_add_rows(layout)[0].1.y - TIMELINE_RULER_H
    }

    /// Which kind of new track a pool drop of `source` at `[x, y]` would make,
    /// if the cursor is in an add row and the source has something to put on
    /// that kind of track.
    pub(crate) fn new_track_drop(&self, source: SourceId, [x, y]: [f32; 2]) -> Option<TrackKind> {
        let layout = self.timeline_layout();
        if x < layout.clips_x {
            return None;
        }
        let (kind, _) = self
            .track_add_rows(&layout)
            .into_iter()
            .find(|(_, row)| row.contains([x, y]))?;
        let fits = match kind {
            TrackKind::Video => self.media.has_video(source),
            TrackKind::Audio => self.media.audio_duration(source).is_some(),
        };
        fits.then_some(kind)
    }

    /// The remove button in an empty track's header, for every track that can
    /// go: empty, and not the last of its kind.
    pub(crate) fn track_remove_buttons(&self, layout: &TimelineLayout) -> Vec<(usize, Rect)> {
        let mut out = Vec::new();
        for (i, track) in self.timeline.tracks.iter().enumerate() {
            if !track.clips.is_empty() || self.n_tracks_of(track.kind) <= 1 {
                continue;
            }
            let y = self.lane_top(i, layout);
            out.push((i, track_header_button(y, layout.lane_h)));
        }
        out
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
        let ruler_top = self.ruler_top(&layout);
        if cursor_y >= ruler_top
            && cursor_y < ruler_top + TIMELINE_RULER_H
            && cursor_x >= layout.clips_x
        {
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
            // Before the fades and trims, which it sits clear of anyway, and
            // the body it sits on.
            if !clip.effects.is_empty() {
                let badge = fx_badge_rect(x0.max(layout.clips_x), x1, lane_y);
                if badge.is_some_and(|b| b.contains([cursor_x, cursor_y])) {
                    return TimelineHit::ClipEffects { track: track_idx, idx: i };
                }
            }
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
                if self.is_selected(clip.id)
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
        let (track, idx) = self.primary_selected()?;
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

    /// Every pool entry that fits in the panel, with where it sits.
    pub(crate) fn pool_items(&self) -> Vec<(SourceId, PoolCell)> {
        let ids = self.media.ids();
        let cells = pool_cells(self.pool_view, self.media_pool_w(), self.timeline_top(), ids.len());
        ids.iter().copied().zip(cells).collect()
    }

    pub(crate) fn pool_hit(&self, cursor_x: f32, cursor_y: f32) -> Option<SourceId> {
        self.pool_items()
            .into_iter()
            .find(|(_, c)| c.cell.contains([cursor_x, cursor_y]))
            .map(|(id, _)| id)
    }

    /// The video clip `(track, idx)` stands for when it comes to effects:
    /// itself on a video track, its linked picture on an audio one — the sound
    /// of a clip has no picture to blur, but clicking it is still a way of
    /// pointing at that clip. `None` for audio with no picture beside it.
    pub(crate) fn effects_clip_for(&self, track: usize, idx: usize) -> Option<(usize, usize)> {
        if self.timeline.tracks[track].kind == TrackKind::Video {
            return Some((track, idx));
        }
        self.linked_siblings(track, idx)
            .into_iter()
            .find(|&(t, _)| self.timeline.tracks[t].kind == TrackKind::Video)
    }

    /// The clip whose effects the effects tab lists: the one picked last.
    pub(crate) fn effects_target(&self) -> Option<(usize, usize)> {
        let (track, idx) = self.primary_selected()?;
        self.effects_clip_for(track, idx)
    }

    /// The effects tab, laid out for the clip it is showing.
    pub(crate) fn effects_panel(&self) -> EffectsPanel {
        let target = self.effects_target().map(|(t, i)| &self.timeline.tracks[t].clips[i]);
        effects_panel(
            self.media_pool_w(),
            self.timeline_top(),
            &EffectKind::ALL,
            self.effects_title().map(|(_, t)| t.text.split('\n').count()),
            target.map(|c| &c.effects),
            self.effects_scroll,
        )
    }

    /// The title the effects tab is showing the settings of, when the clip it
    /// is showing is a title: its source and the title as it stands.
    pub(crate) fn effects_title(&self) -> Option<(SourceId, &Title)> {
        let (t, i) = self.effects_target()?;
        let source = self.timeline.tracks[t].clips[i].source;
        self.media.title(source).map(|title| (source, title))
    }

    /// The font list, laid out under the font box, while it is open.
    pub(crate) fn font_menu(&self) -> Option<FontMenu> {
        let scroll = self.font_menu?;
        if self.pool_tab != PoolTab::Effects {
            return None;
        }
        let anchor = self.effects_panel().font_field()?;
        Some(font_menu(anchor, self.logical_size(), fonts::choice_count(), scroll))
    }

    pub(crate) fn pool_close_hit(&self, cursor_x: f32, cursor_y: f32) -> Option<SourceId> {
        self.pool_items()
            .into_iter()
            .find(|(_, c)| {
                pool_row_close_rect(c.cell.x, c.cell.y, c.cell.w).contains([cursor_x, cursor_y])
            })
            .map(|(id, _)| id)
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

    /// The stack reads top to bottom as ruler, video add row, video lanes,
    /// audio lanes, audio add row — flush, nothing overlapping — and stays
    /// inside the panel however lopsided the track counts get.
    #[test]
    fn the_track_stack_is_centred_and_its_add_rows_hug_the_lanes() {
        let (top, area) = (500.0, 400.0);
        for (n_video, n_audio) in [(2, 2), (5, 1), (1, 4), (3, 0)] {
            let n = n_video + n_audio;
            let lane_h = compute_lane_height(area - TRACKS_CHROME_H, n);
            let center = lanes_center_y(top, area, lane_h, n_video, n);
            let [(_, video_row), (_, audio_row)] =
                track_add_rows(center, lane_h, n_video, n_audio, 800.0);
            let topmost = topmost_lane_top(center, lane_h, n_video);
            assert_eq!(video_row.y + video_row.h, topmost, "{n_video}v {n_audio}a");
            let ruler_top = video_row.y - TIMELINE_RULER_H;
            assert!(ruler_top >= top, "{n_video}v {n_audio}a: ruler at {ruler_top}");
            assert!(audio_row.y + audio_row.h <= top + area + 1.0, "{n_video}v {n_audio}a");
            if n_audio > 0 {
                let last = lane_y(center, lane_h, n_audio - 1, TrackKind::Audio);
                assert_eq!(audio_row.y, last + lane_h);
            }
            // Centred: the space above the ruler matches the space below the
            // audio row, to the rounding of the centre line.
            let above = ruler_top - top;
            let below = top + area - (audio_row.y + audio_row.h);
            assert!((above - below).abs() <= 1.0, "{n_video}v {n_audio}a: {above} vs {below}");
        }
    }

    #[test]
    fn a_stack_taller_than_the_panel_runs_off_the_bottom_not_the_top() {
        let (top, area) = (500.0, 200.0);
        let lane_h = compute_lane_height(area - TRACKS_CHROME_H, 12);
        let center = lanes_center_y(top, area, lane_h, 8, 12);
        let [(_, video_row), _] = track_add_rows(center, lane_h, 8, 4, 800.0);
        assert_eq!(video_row.y - TIMELINE_RULER_H, top);
    }

    #[test]
    fn the_effects_tab_stacks_cards_then_the_clips_effects_without_overlap() {
        use crate::effects::{EffectKind, EffectStack};
        let mut stack = EffectStack::default();
        stack.push(EffectKind::GaussianBlur.instance());
        stack.push(EffectKind::GaussianBlur.instance());
        let panel = effects_panel(400.0, 2000.0, &EffectKind::ALL, None, Some(&stack), 0.0);
        assert_eq!(panel.library.len(), EffectKind::ALL.len());
        let card = panel.library[0].card;
        // The card's apply button sits on its picture, and the card is a
        // media grid tile exactly.
        let apply = panel.library[0].apply;
        assert!(card.thumb.contains([apply.x, apply.y]));
        assert_eq!(card.cell, pool_cells(PoolView::Grid, 400.0, 2000.0, 1)[0].cell);
        assert!(panel.clip_heading_y > card.cell.y + card.cell.h);
        assert_eq!(panel.stack.len(), 2);
        let first_bottom = panel.stack[0].params.last().unwrap().row;
        assert!(panel.stack[1].row.y >= first_bottom.y + first_bottom.h);
        // Every press lands on what is drawn there.
        assert_eq!(panel.hit([card.cell.x + 2.0, card.cell.y + 2.0]),
            Some(EffectsHit::Library(EffectKind::GaussianBlur)));
        assert_eq!(panel.hit([apply.x + 2.0, apply.y + 2.0]),
            Some(EffectsHit::Apply(EffectKind::GaussianBlur)));
        let toggle = panel.stack[1].toggle;
        assert_eq!(panel.hit([toggle.x + 2.0, toggle.y + 2.0]), Some(EffectsHit::Toggle { index: 1 }));
    }

    #[test]
    fn an_effects_tab_too_short_for_its_rows_scrolls_to_reach_them() {
        use crate::effects::{EffectKind, EffectStack};
        let mut stack = EffectStack::default();
        for _ in 0..8 {
            stack.push(EffectKind::GaussianBlur.instance());
        }
        let top = effects_panel(400.0, 420.0, &EffectKind::ALL, Some(1), Some(&stack), 0.0);
        assert_eq!(top.stack.len(), 8, "rows are scrolled to, not dropped");
        assert!(top.max_scroll > 0.0);
        // Scrolled past the end, it stops at the end: the last row's bottom
        // sits inside the panel.
        let end = effects_panel(400.0, 420.0, &EffectKind::ALL, Some(1), Some(&stack), 1e6);
        let last = end.stack[7].params.last().unwrap().row;
        assert!(last.y + last.h <= 420.0);
        assert!(last.y + last.h > 420.0 - EFFECT_ROW_H * 2.0);
        // What is scrolled out of sight takes no presses, even where a row
        // of it now sits above the panel's top.
        let card = end.library[0].card.cell;
        assert!(card.y + card.h < POOL_LIST_TOP);
        assert_eq!(end.hit([card.x + 2.0, card.y + card.h - 2.0]), None);
    }

    #[test]
    fn a_titles_settings_sit_between_the_heading_and_its_effects() {
        use crate::effects::{EffectKind, EffectStack};
        let mut stack = EffectStack::default();
        stack.push(EffectKind::GaussianBlur.instance());
        let panel = effects_panel(400.0, 2000.0, &EffectKind::ALL, Some(1), Some(&stack), 0.0);
        let controls: Vec<TitleControl> = panel.title.iter().map(|r| r.control).collect();
        assert_eq!(controls.len(), 11);
        assert_eq!(controls[0], TitleControl::Text);
        assert!(panel.title[0].row.y >= panel.clip_heading_y + EFFECTS_SECTION_H);
        let last = panel.title.last().unwrap().row;
        assert!(panel.stack[0].row.y >= last.y + last.h);
        for pair in panel.title.windows(2) {
            assert!(pair[1].row.y >= pair[0].row.y + pair[0].row.h);
        }
        // Each control answers to a press on it.
        let at = |c: TitleControl| panel.title.iter().find(|r| r.control == c).unwrap();
        let f = at(TitleControl::Text).field;
        assert_eq!(panel.hit([f.x + 2.0, f.y + 2.0]), Some(EffectsHit::TitleText));
        let f = at(TitleControl::Font).field;
        assert_eq!(panel.hit([f.x + 2.0, f.y + 2.0]), Some(EffectsHit::TitleFont));
        let size = at(TitleControl::Number(TitleNumber::Size));
        assert_eq!(
            panel.hit([size.field.x + 4.0, size.row.y + 2.0]),
            Some(EffectsHit::TitleNumber { field: TitleNumber::Size, slider: size.field })
        );
        let swatches = swatch_rects(at(TitleControl::Color(TitleColor::Outline)).field);
        assert_eq!(
            panel.hit([swatches[3].x + 2.0, swatches[3].y + 2.0]),
            Some(EffectsHit::TitleSwatch { field: TitleColor::Outline, swatch: 3 })
        );
        let toggle = at(TitleControl::Toggle(TitlePart::Shadow)).row;
        assert_eq!(
            panel.hit([toggle.x + 100.0, toggle.y + 2.0]),
            Some(EffectsHit::TitleToggle(TitlePart::Shadow))
        );
        // A clip that is not a title has none of it.
        let plain = effects_panel(400.0, 2000.0, &EffectKind::ALL, None, Some(&stack), 0.0);
        assert!(plain.title.is_empty());
    }

    #[test]
    fn the_text_box_grows_with_the_lines_and_stops_at_a_cap() {
        use crate::effects::EffectKind;
        let field = |lines| {
            let p = effects_panel(400.0, 2000.0, &EffectKind::ALL, Some(lines), None, 0.0);
            p.title.iter().find(|r| r.control == TitleControl::Text).unwrap().field.h
        };
        assert!(field(3) > field(1));
        assert_eq!(field(3) - field(2), TITLE_TEXT_LINE_H);
        assert_eq!(field(TITLE_TEXT_MAX_LINES), field(TITLE_TEXT_MAX_LINES + 5));
        // Every swatch fits in its row, and none overlap.
        let p = effects_panel(400.0, 2000.0, &EffectKind::ALL, Some(1), None, 0.0);
        let row = p.title.iter().find(|r| matches!(r.control, TitleControl::Color(_))).unwrap();
        let rects = swatch_rects(row.field);
        for (i, a) in rects.iter().enumerate() {
            assert!(a.y >= row.row.y && a.y + a.h <= row.row.y + row.row.h);
            assert!(a.x + a.w <= row.row.x + row.row.w);
            for b in &rects[i + 1..] {
                assert!(a.x + a.w <= b.x || b.x + b.w <= a.x || a.y + a.h <= b.y || b.y + b.h <= a.y);
            }
        }
    }

    #[test]
    fn the_font_list_stays_inside_the_window_and_scrolls() {
        let anchor = Rect { x: 100.0, y: 200.0, w: 180.0, h: TITLE_FIELD_H };
        let menu = font_menu(anchor, [1200.0, 800.0], 300, 0);
        assert_eq!(menu.rows.len(), FONT_MENU_ROWS);
        assert!(menu.rect.y >= anchor.y + anchor.h);
        assert_eq!(menu.max_scroll, 300 - FONT_MENU_ROWS);
        // Near the bottom of the window it opens upwards instead.
        let low = Rect { y: 760.0, ..anchor };
        let up = font_menu(low, [1200.0, 800.0], 300, 0);
        assert!(up.rect.y + up.rect.h <= low.y);
        assert!(up.rect.y >= 0.0);
        // Scrolled past the end, the last entry is the last row.
        let end = font_menu(anchor, [1200.0, 800.0], 300, 10_000);
        assert_eq!(end.rows.last().unwrap().0, 299);
        for (_, r) in &menu.rows {
            assert!(r.x + r.w <= 1200.0 && r.y + r.h <= 800.0);
        }
    }

    #[test]
    fn a_grid_fills_the_pool_width_with_whole_tiles() {
        for pool_w in [POOL_MIN_W, 330.0, 500.0, 900.0] {
            let cells = pool_cells(PoolView::Grid, pool_w, 10_000.0, 12);
            let right = cells.iter().map(|c| c.cell.x + c.cell.w).fold(0.0, f32::max);
            assert!(right <= pool_w - LABEL_PAD, "{pool_w}: runs to {right}");
            for c in &cells {
                assert!(c.cell.w >= POOL_TILE_MIN_W || pool_w < POOL_TILE_MIN_W + LABEL_PAD * 3.0);
            }
        }
        // Wide enough for three at the minimum, so three it is.
        let three = LABEL_PAD * 2.0 + POOL_TILE_MIN_W * 3.0 + POOL_TILE_GAP * 2.0;
        let cells = pool_cells(PoolView::Grid, three, 10_000.0, 4);
        assert_eq!(cells[2].cell.y, cells[0].cell.y);
        assert!(cells[3].cell.y > cells[0].cell.y);
    }

    #[test]
    fn no_two_pool_cells_overlap_in_either_view() {
        for view in [PoolView::List, PoolView::Grid] {
            let cells = pool_cells(view, 480.0, 10_000.0, 9);
            assert_eq!(cells.len(), 9);
            for (i, a) in cells.iter().enumerate() {
                assert!(a.cell.contains([a.thumb.x, a.thumb.y]));
                assert!(a.cell.contains([a.thumb.x + a.thumb.w, a.thumb.y + a.thumb.h]));
                for b in &cells[i + 1..] {
                    let apart = a.cell.x + a.cell.w < b.cell.x
                        || b.cell.x + b.cell.w < a.cell.x
                        || a.cell.y + a.cell.h < b.cell.y
                        || b.cell.y + b.cell.h < a.cell.y;
                    assert!(apart, "{view:?}: {a:?} overlaps {b:?}");
                }
            }
        }
    }

    #[test]
    fn a_pool_cell_that_would_run_off_the_panel_is_left_out() {
        for view in [PoolView::List, PoolView::Grid] {
            let cells = pool_cells(view, 400.0, 300.0, 50);
            assert!(!cells.is_empty() && cells.len() < 50);
            assert!(cells.iter().all(|c| c.cell.y + c.cell.h <= 300.0));
        }
    }

    #[test]
    fn the_zoom_slider_reads_back_the_span_it_sets() {
        for content in [MIN_CONTENT_SPAN, 12.0, 600.0, 7200.0] {
            for fraction in [0.0, 0.1, 0.5, 0.9, 1.0] {
                let back = zoom_fraction(zoom_span(fraction, content), content);
                assert!((back - fraction).abs() < 1e-9, "{content}: {fraction} -> {back}");
            }
            assert_eq!(zoom_span(0.0, content), content);
            assert!((zoom_span(1.0, content) - MIN_VIEW_SPAN).abs() < 1e-9);
        }
    }

    #[test]
    fn the_zoom_slider_steps_by_ratios_not_seconds() {
        // The same distance along the slider is the same zoom factor wherever
        // it is taken, which is what makes the slider feel like the wheel.
        let content = 600.0;
        let a = zoom_span(0.2, content) / zoom_span(0.3, content);
        let b = zoom_span(0.7, content) / zoom_span(0.8, content);
        assert!((a - b).abs() < 1e-9);
    }
}
