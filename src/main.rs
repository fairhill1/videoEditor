mod audio;
mod export;
mod media;
mod project;
mod quad;
mod text;
mod timeline;
mod ui;
mod video;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::{ElementState, KeyEvent, MouseButton, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, OwnedDisplayHandle},
    keyboard::{KeyCode, ModifiersState, PhysicalKey},
    window::{CursorIcon, Window, WindowId},
};

use audio::AudioEngine;
use export::{ExportJob, ExportRequest, Outcome, VideoSpec};
use media::MediaPool;
use quad::{Quad, QuadRenderer};
use text::TextRenderer;
use timeline::{Clip, SourceId, Timeline, TimelineSnapshot, Track, TrackKind};
use ui::{draw_button, draw_menu_row, draw_tooltip, BtnState, Rect, TooltipSide};

// Lucide glyphs, from the subset in `assets/fonts/lucide-subset.ttf`. Named
// here rather than spelled inline so a codepoint appears exactly once — they
// are unreadable on sight, and `tools/build-icon-font.sh` is what keeps this
// list and the font in step.
const ICON_PREV_EDIT: char = '\u{E243}'; // chevron-first
const ICON_PREV_FRAME: char = '\u{E06E}'; // chevron-left
const ICON_PLAY: char = '\u{E13C}'; // play
const ICON_PAUSE: char = '\u{E12E}'; // pause
const ICON_NEXT_FRAME: char = '\u{E06F}'; // chevron-right
const ICON_NEXT_EDIT: char = '\u{E244}'; // chevron-last
const ICON_SPLIT: char = '\u{E3B6}'; // square-split-horizontal
const ICON_DELETE: char = '\u{E18D}'; // trash
const ICON_UNDO: char = '\u{E19B}'; // undo
const ICON_REDO: char = '\u{E143}'; // redo
const ICON_SNAP: char = '\u{E2B5}'; // magnet
const ICON_RENDER: char = '\u{E0D0}'; // film
const ICON_STOP: char = '\u{E167}'; // square
const ICON_IMPORT: char = '\u{E22F}'; // import
const ICON_CLOSE: char = '\u{E1B2}'; // x
const ICON_SETTINGS: char = '\u{E154}'; // settings (gear)
const ICON_OPEN: char = '\u{E247}'; // folder-open
const ICON_SAVE: char = '\u{E14D}'; // save (floppy)

// Starting layout split ratios. Both are draggable at runtime and live on
// `State` from then on; these are only where a fresh session begins.
const TOP_BOTTOM_SPLIT: f32 = 0.55;
const MEDIA_PREVIEW_SPLIT: f32 = 0.28;

// Splitter behavior.
/// How far either side of a divider counts as grabbing it. Generous next to
/// `CLIP_EDGE_GRAB_PX`, because missing a splitter is worse than missing a trim
/// handle: the click lands on whatever is behind it and scrubs or deselects.
const SPLITTER_GRAB_PX: f32 = 5.0;
/// Width of the band drawn over a divider while it is hovered or dragged. Wider
/// than the 1pt edge it covers, so the divider visibly becomes a handle rather
/// than just changing color, and centered on that edge so nothing shifts.
const SPLITTER_ACTIVE_W: f32 = 3.0;
const SPLITTER_ACTIVE_COLOR: [f32; 4] = [0.45, 0.45, 0.53, 1.0];
/// Floors for the four panels a splitter can squeeze, in points.
///
/// The preview minimum is set by its transport bar rather than the picture:
/// five buttons plus gaps plus the timecode readout is the point below which
/// controls would start overlapping, and a preview can always letterbox.
const POOL_MIN_W: f32 = 160.0;
const PREVIEW_MIN_W: f32 = 340.0;
/// Enough for the transport bar plus a sliver of picture above it.
const TOP_MIN_H: f32 = TRANSPORT_BAR_H + 60.0;
/// Toolbar, ruler and one lane at its minimum height — below this the timeline
/// stops being a timeline.
const TIMELINE_MIN_H: f32 = TIMELINE_TOP_PAD + TIMELINE_RULER_H + TRACK_LANE_MIN_H;

// Surface elevation scale (sRGB), darkest first. Steps widen as they climb:
// down near black a small numeric difference is imperceptible, so the low tiers
// need real distance between them or the whole window reads as one flat mass.
// Every panel picks a tier rather than its own value, so surfaces at the same
// conceptual depth actually match.
const SURFACE_WELL: [f32; 4] = [0.03, 0.03, 0.04, 1.0]; // content the app displays into
const SURFACE_LANE: [f32; 4] = [0.05, 0.05, 0.06, 1.0]; // wells that hold clips
const SURFACE_BASE: [f32; 4] = [0.09, 0.09, 0.11, 1.0]; // body behind the wells
const SURFACE_PANEL: [f32; 4] = [0.15, 0.15, 0.18, 1.0]; // chrome that holds controls

// Panel assignments.
const MEDIA_POOL_COLOR: [f32; 4] = SURFACE_PANEL;
const PREVIEW_COLOR: [f32; 4] = SURFACE_WELL;
/// The canvas itself, sitting inside the preview well. True black rather than
/// the well's near-black: it's picture area, and it has to read as distinct
/// from the panel it floats in even when no clip is playing.
const CANVAS_COLOR: [f32; 4] = [0.0, 0.0, 0.0, 1.0];
const TIMELINE_COLOR: [f32; 4] = SURFACE_BASE;
const LANE_COLOR: [f32; 4] = SURFACE_LANE;
// Edge between two panels, lighter than both — the flat-UI way to define a
// boundary, standing in for the highlight/shadow pair of a bevel.
const PANEL_BORDER_COLOR: [f32; 4] = [0.28, 0.28, 0.33, 1.0];
// Softer line for divisions *within* a panel, which shouldn't read as loudly as
// the panel's own edges.
const DIVIDER_COLOR: [f32; 4] = [0.20, 0.20, 0.24, 1.0];
// Clip fills. Blue is video and green is audio, and both are darker than they
// look like they want to be, because `CLIP_LABEL_COLOR` sits directly on them:
// at the original lightness the filename ran 4.3:1 on video and 3.1:1 on audio,
// under the 4.5:1 WCAG AA wants for text this size. Each fill is the *lightest*
// value that clears it, so they give up no more saturation than they must.
//
// Derived by scaling all three channels by one factor, which leaves hue and
// saturation untouched and moves only lightness — the blue is still the same
// blue. The budget is set by the selected state, not the resting one:
// `CLIP_SELECTED_LIFT` blends the fill toward white, so the label's worst case
// is a selected clip, and that is the case these were solved for.
//
// Audio stays the lighter of the two by the same 1.5x in luminance it always
// was. That gap is the only cue separating them for a viewer who can't tell the
// hues apart, so matching them in lightness would have cost more than it saved.
const VIDEO_CLIP_COLOR: [f32; 4] = [0.19, 0.28, 0.44, 1.0];
const AUDIO_CLIP_COLOR: [f32; 4] = [0.19, 0.38, 0.25, 1.0];
// Outline around each clip. Two butt-joined clips show it twice, so a split
// reads as a 2px seam.
const CLIP_BORDER_PX: f32 = 1.0;
const CLIP_BORDER_DARKEN: f32 = 0.40;
// Selection accent. Lighter than the toggle orange so it still separates from
// the saturated blue and green clip fills it has to sit on.
const CLIP_SELECTED_BORDER: [f32; 4] = [1.0, 0.72, 0.30, 1.0];
const CLIP_SELECTED_BORDER_PX: f32 = 2.0;
const CLIP_SELECTED_LIFT: f32 = 0.14;
// How close a dragged edge must come to latch onto a snap target. In pixels
// rather than seconds so the pull feels the same however long the timeline is.
const SNAP_PX: f32 = 8.0;
const AUDIO_WAVE_COLOR: [f32; 4] = [0.75, 0.95, 0.80, 0.95];
const CLIP_LABEL_COLOR: [f32; 4] = [0.95, 0.95, 0.98, 1.0];
const LABEL_COLOR: [f32; 4] = [0.72, 0.72, 0.78, 1.0];
// Was dim enough that V1/A1 were a squint to read; a track's identity should be
// legible at a glance, not decorative.
const TRACK_LABEL_COLOR: [f32; 4] = [0.62, 0.62, 0.68, 1.0];
// The type scale. Every font size below is one of these steps, so a new piece
// of chrome picks a step rather than inventing a value a half point off one
// that already exists.
//
// `TYPE_SM` is the floor: nothing renders text smaller. The ruler labels and the
// pool's format line used to sit a point under it and read as a squint rather
// than as small print, and anything reaching lower is reaching for the same
// mistake. These are points, so the floor holds its physical size across
// displays — see [`State::scale`].
const TYPE_SM: f32 = 11.0;
const TYPE_MD: f32 = 12.0;
const TYPE_LG: f32 = 13.0;
const TYPE_XL: f32 = 14.0;
/// Icon glyphs sit above the text steps: a Lucide glyph fills its em box where
/// a letter leaves clearance above and below, so matching a text size optically
/// means exceeding it numerically.
const TYPE_ICON: f32 = 16.0;

const LABEL_SIZE: f32 = TYPE_LG;
const CLIP_LABEL_SIZE: f32 = TYPE_SM;
const LABEL_PAD: f32 = 10.0;
const PLAYHEAD_COLOR: [f32; 4] = [0.95, 0.35, 0.35, 1.0];
const PLAYHEAD_WIDTH: f32 = 2.0;
const TIMER_SIZE: f32 = TYPE_XL;
const TIMER_COLOR: [f32; 4] = [0.95, 0.95, 0.98, 1.0];
// Transport bar between preview and timeline; holds prev/play/next + timer.
const TRANSPORT_BAR_H: f32 = 40.0;
// Panel tier, not well tier: it holds controls, and at its old near-black value
// it dissolved into the preview above it.
const TRANSPORT_BAR_COLOR: [f32; 4] = SURFACE_PANEL;
// Buttons are icon-only, so they no longer need to fit a word — square-ish
// keeps the glyph optically centered and tightens both toolbars considerably.
const TRANSPORT_BTN_W: f32 = 32.0;
const TRANSPORT_BTN_H: f32 = 26.0;
const TRANSPORT_GAP: f32 = 8.0;
const TRANSPORT_ICON_SIZE: f32 = TYPE_ICON;
const TRANSPORT_TOOLTIP_SIZE: f32 = TYPE_SM;

// Status readout, occupying the toolbar row between the edit buttons and the
// right-aligned Export button. Doubles as the progress bar while a render runs
// and the message line the rest of the time, so a render's progress and the
// result of a save never fight for the same space.
const EXPORT_BTN_W: f32 = TRANSPORT_BTN_W;
const EXPORT_READOUT_W: f32 = 190.0;
const EXPORT_READOUT_GAP: f32 = 10.0;
const EXPORT_BAR_H: f32 = 4.0;
const EXPORT_BAR_TRACK: [f32; 4] = [0.10, 0.10, 0.13, 1.0];
const EXPORT_BAR_FILL: [f32; 4] = [0.95, 0.55, 0.15, 1.0];
const STATUS_SIZE: f32 = TYPE_SM;
const STATUS_OK: [f32; 4] = [0.60, 0.85, 0.65, 1.0];
const STATUS_ERR: [f32; 4] = [0.92, 0.55, 0.55, 1.0];
const STATUS_INFO: [f32; 4] = [0.72, 0.72, 0.78, 1.0];
/// How long a status message lingers. Long enough to read, short enough that
/// it clears itself instead of needing a dismiss affordance.
const STATUS_SECONDS: f64 = 8.0;
/// Canvas format used when nothing better is known: an empty timeline, or a
/// source that has vanished from the pool mid-session.
const EXPORT_FALLBACK_SIZE: (u32, u32) = (1920, 1080);
const EXPORT_FALLBACK_FPS: f64 = 30.0;

// Project canvas presets, offered by the settings popup.
const RES_PRESETS: [(u32, u32); 4] = [(3840, 2160), (2560, 1440), (1920, 1080), (1280, 720)];
/// Grouped by column as they're laid out: film rates, standard, high. Keeping
/// the grouping in the data means the popup's columns can't drift from the
/// meaning they're supposed to carry.
const FPS_COLUMNS: [&[f64]; 3] = [
    &[23.976, 24.0],
    &[25.0, 29.97, 30.0],
    &[50.0, 59.94, 60.0],
];

// Project settings popup.
const MENU_BG: [f32; 4] = [0.13, 0.13, 0.16, 1.0];
const MENU_BORDER: [f32; 4] = [0.34, 0.34, 0.40, 1.0];
const MENU_PAD: f32 = 10.0;
const MENU_ROW_H: f32 = 22.0;
const MENU_ROW_GAP: f32 = 1.0;
const MENU_SECTION_GAP: f32 = 12.0;
const MENU_HEADER_SIZE: f32 = TYPE_SM;
const MENU_HEADER_COLOR: [f32; 4] = [0.62, 0.62, 0.68, 1.0];
const MENU_ROW_SIZE: f32 = TYPE_MD;
const MENU_RES_W: f32 = 132.0;
const MENU_FPS_COL_W: f32 = 60.0;
const MENU_FPS_COL_GAP: f32 = 4.0;
/// Gap between the gear and the popup it opens, and the same faked soft shadow
/// the tooltips use — see the note on `TOOLTIP_SHADOW` in `ui.rs`.
const MENU_GAP: f32 = 7.0;
const MENU_SHADOW: [f32; 4] = [0.0, 0.0, 0.0, 0.10];
const MENU_SHADOW_STEPS: i32 = 3;

// Timeline panel layout.
// Lane height is computed per-frame to fill the timeline area; these bounds
// keep it readable with one track and prevent chunkiness at high counts.
const TRACK_LANE_MIN_H: f32 = 32.0;
const TRACK_LANE_MAX_H: f32 = 88.0;
const TRACK_LANE_FILL: f32 = 0.9; // fraction of tracks-area height the lanes+gaps try to fill
const TRACK_LANE_GAP: f32 = 2.0;
const TRACK_HEADER_WIDTH: f32 = 48.0;
// Height of the toolbar band above the ruler, holding the "TIMELINE" label and
// its buttons. Sized as the button height plus even breathing room either side
// rather than hugging it — at the old 30 the 26px buttons cleared the panel
// edge by 2px and looked jammed against it.
const TIMELINE_TOP_PAD: f32 = 46.0;
const TIMELINE_RULER_H: f32 = 22.0; // scrub strip between the title bar and lanes
const TIMELINE_RULER_COLOR: [f32; 4] = SURFACE_PANEL;
const TIMELINE_RULER_TICK_COLOR: [f32; 4] = [0.58, 0.58, 0.64, 1.0];
const TIMELINE_RULER_LABEL_COLOR: [f32; 4] = [0.72, 0.72, 0.78, 1.0];
const TIMELINE_RULER_LABEL_SIZE: f32 = TYPE_SM;
const TIMELINE_RULER_TICK_H: f32 = 6.0;

// Media pool list layout.
const POOL_LIST_TOP: f32 = 36.0; // below the MEDIA POOL label
const POOL_ROW_HEIGHT: f32 = 64.0;
const POOL_ROW_GAP: f32 = 4.0;
const POOL_ROW_PAD: f32 = 6.0;
const POOL_ROW_COLOR: [f32; 4] = [0.20, 0.20, 0.24, 1.0];
const POOL_ITEM_NAME_SIZE: f32 = TYPE_MD;
const POOL_ITEM_META_SIZE: f32 = TYPE_SM;
/// Format line under each pool row's filename. Dimmer than the name and a size
/// down, so a row still reads as "a clip called X" at a glance rather than as
/// two competing lines.
const POOL_ITEM_META_COLOR: [f32; 4] = LABEL_COLOR;
const POOL_ITEM_META_GAP: f32 = 5.0;
// Thumbnail slot inside each row — fixed ~16:9 slot, actual thumb is
// letterboxed into it preserving source aspect.
const POOL_THUMB_W: f32 = 92.0;
const POOL_THUMB_H: f32 = POOL_ROW_HEIGHT - POOL_ROW_PAD * 2.0;
const POOL_THUMB_BG: [f32; 4] = [0.08, 0.08, 0.10, 1.0];
const POOL_DUR_BG: [f32; 4] = [0.0, 0.0, 0.0, 0.65];
const POOL_DUR_TEXT: [f32; 4] = [0.95, 0.95, 0.98, 1.0];
/// The close button's hit box, not a font size — hence `BOX`. Every `_SIZE` in
/// this file is a step on the type scale, which is what makes a stray literal
/// on one of them easy to spot.
const POOL_CLOSE_BOX: f32 = 18.0;
const POOL_CLOSE_INSET: f32 = 3.0;
const POOL_CLOSE_BG: [f32; 4] = [0.0, 0.0, 0.0, 0.70];
const POOL_CLOSE_BG_HOVER: [f32; 4] = [0.65, 0.25, 0.25, 0.95];
const POOL_CLOSE_LABEL_SIZE: f32 = TYPE_LG;

// Clip interaction.
const CLIP_EDGE_GRAB_PX: f32 = 6.0;
const MIN_CLIP_DURATION: f64 = 0.05; // seconds — keeps trim from zeroing a clip
/// The drag ghost is a clip's own fill at reduced alpha. Derived from that fill
/// rather than restated, because these were literal copies of it and would have
/// gone on drawing the old, lower-contrast blue and green after the fills moved.
const DRAG_GHOST_ALPHA: f32 = 0.55;
const DRAG_GHOST_VIDEO_COLOR: [f32; 4] = [
    VIDEO_CLIP_COLOR[0],
    VIDEO_CLIP_COLOR[1],
    VIDEO_CLIP_COLOR[2],
    DRAG_GHOST_ALPHA,
];
const DRAG_GHOST_AUDIO_COLOR: [f32; 4] = [
    AUDIO_CLIP_COLOR[0],
    AUDIO_CLIP_COLOR[1],
    AUDIO_CLIP_COLOR[2],
    DRAG_GHOST_ALPHA,
];

#[derive(Copy, Clone, Debug)]
enum DragMode {
    None,
    Scrub,
    PoolDrag { source: SourceId },
    ClipMove { track: usize, idx: usize, grab_t_offset: f64 },
    ClipTrimLeft { track: usize, idx: usize },
    ClipTrimRight { track: usize, idx: usize },
    /// Dragging a panel divider. Deliberately outside the undo system: where
    /// the panels sit is a view preference, and burying a real edit one step
    /// further back in the history every time you resize would be maddening.
    Splitter(Splitter),
}

/// The project canvas: the picture every clip is composed onto, and the format
/// the export is encoded in.
///
/// This is the frame the preview draws and clips are fitted into, not merely an
/// export setting — when clips gain a position/scale/crop, those are expressed
/// relative to this.
#[derive(Copy, Clone, PartialEq, Debug)]
struct Canvas {
    width: u32,
    height: u32,
    fps: f64,
}

impl Canvas {
    /// Where a `sw x sh` source lands on the canvas, in canvas pixels.
    ///
    /// Currently always an aspect-preserving fit, centered. This is the seam a
    /// per-clip transform replaces: once clips carry their own position, scale
    /// and crop, this becomes "apply that clip's transform" and both the
    /// preview and the export follow automatically, because both compose
    /// through here rather than each doing their own arithmetic.
    fn fit(&self, sw: f32, sh: f32) -> (f32, f32, f32, f32) {
        let (cw, ch) = (self.width as f32, self.height as f32);
        if sw <= 0.0 || sh <= 0.0 {
            return (0.0, 0.0, cw, ch);
        }
        let scale = (cw / sw).min(ch / sh);
        let (w, h) = (sw * scale, sh * scale);
        ((cw - w) * 0.5, (ch - h) * 0.5, w, h)
    }
}

/// A canvas dimension the user has pinned, or `Auto` to follow the footage.
///
/// `Auto` is the default and reproduces the original behavior — take the format
/// of the first clip on the timeline. It stays a live mode rather than being
/// snapshotted at import, so an empty project that gains its first clip adopts
/// that clip, and the user can pin the format the moment that guess is wrong.
///
/// Saving the mode rather than the resolved value is the whole point: an `Auto`
/// project reopened after its footage was swapped follows the new footage.
#[derive(Copy, Clone, PartialEq, Debug, Serialize, Deserialize)]
enum Setting<T> {
    Auto,
    Fixed(T),
}

/// One clickable row in the settings popup.
#[derive(Copy, Clone, PartialEq, Debug)]
enum ProjectChoice {
    Resolution(Setting<(u32, u32)>),
    Fps(Setting<f64>),
}

/// The two panel dividers, each named for what it separates.
#[derive(Copy, Clone, Debug, PartialEq)]
enum Splitter {
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
fn resolve_split(frac: f32, total: f32, min_before: f32, min_after: f32) -> f32 {
    if total <= min_before + min_after {
        // Not enough room for both minimums. Divide what there is in the ratio
        // of the minimums themselves: neither panel vanishes, and the result is
        // stable rather than depending on which one we chose to satisfy first.
        return (total * min_before / (min_before + min_after)).round();
    }
    (frac * total).round().clamp(min_before, total - min_after)
}

/// One undo step: everything a user edit can change. Pool membership rides
/// along with the timeline because deleting a pool item also deletes its
/// clips — undoing that has to put both back in one move.
#[derive(Clone, PartialEq)]
struct EditSnapshot {
    timeline: TimelineSnapshot,
    pool_order: Vec<SourceId>,
}

/// Retained undo steps. Snapshots are small, but a long session shouldn't grow
/// without bound; the oldest step is dropped past this.
const UNDO_LIMIT: usize = 200;

enum TimelineHit {
    None,
    Ruler,
    Lane { track: usize },
    ClipBody { track: usize, idx: usize, grab_t_offset: f64 },
    ClipTrimLeft { track: usize, idx: usize },
    ClipTrimRight { track: usize, idx: usize },
}

#[derive(Copy, Clone)]
struct TimelineLayout {
    top: f32,
    clips_x: f32,
    clips_w: f32,
    center_y: f32,
    lane_h: f32,
    duration: f64,
}

fn pool_row_close_rect(row_x: f32, row_y: f32, row_w: f32) -> Rect {
    Rect {
        x: row_x + row_w - POOL_CLOSE_INSET - POOL_CLOSE_BOX,
        y: row_y + POOL_CLOSE_INSET,
        w: POOL_CLOSE_BOX,
        h: POOL_CLOSE_BOX,
    }
}

fn nice_tick_interval(pixels_per_sec: f32) -> f64 {
    const TARGET_PX: f32 = 100.0;
    if pixels_per_sec <= 0.0 {
        return 60.0;
    }
    let raw_secs = (TARGET_PX / pixels_per_sec) as f64;
    let nice = [
        0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0,
    ];
    for &v in &nice {
        if v >= raw_secs {
            return v;
        }
    }
    3600.0
}

fn format_tick_label(t: f64, interval: f64) -> String {
    let total_sec = t.max(0.0);
    if interval < 1.0 {
        let total_ms = (total_sec * 1000.0).round() as u64;
        let s_total = total_ms / 1000;
        let cs = (total_ms % 1000) / 10;
        let m = s_total / 60;
        let s = s_total % 60;
        format!("{}:{:02}.{:02}", m, s, cs)
    } else {
        let total = total_sec.round() as u64;
        let h = total / 3600;
        let m = (total / 60) % 60;
        let s = total % 60;
        if h > 0 {
            format!("{}:{:02}:{:02}", h, m, s)
        } else {
            format!("{}:{:02}", m, s)
        }
    }
}

/// Closest latch of any `edge` onto any `target` within `threshold`, expressed
/// as the offset to add to the drag delta. `None` when nothing is in range.
///
/// Every edge competes against every target and the smallest gap wins, so a
/// clip latches by whichever end you brought near a neighbour rather than by a
/// fixed leading edge.
fn nearest_snap(edges: &[f64], targets: &[f64], threshold: f64) -> Option<f64> {
    let mut best: Option<(f64, f64)> = None;
    for &edge in edges {
        for &target in targets {
            let dist = (target - edge).abs();
            if dist <= threshold && best.is_none_or(|(bd, _)| dist < bd) {
                best = Some((dist, target - edge));
            }
        }
    }
    best.map(|(_, adjust)| adjust)
}

/// Blend `c` toward white by `f`, leaving alpha alone.
fn lighten(c: [f32; 4], f: f32) -> [f32; 4] {
    [
        c[0] + (1.0 - c[0]) * f,
        c[1] + (1.0 - c[1]) * f,
        c[2] + (1.0 - c[2]) * f,
        c[3],
    ]
}

fn darken(c: [f32; 4], f: f32) -> [f32; 4] {
    [c[0] * f, c[1] * f, c[2] * f, c[3]]
}

fn topmost_lane_top(center_y: f32, lane_h: f32, n_video: usize) -> f32 {
    let half_gap = TRACK_LANE_GAP * 0.5;
    if n_video == 0 {
        center_y
    } else {
        center_y - half_gap - lane_h * n_video as f32
            - (n_video as f32 - 1.0) * TRACK_LANE_GAP
    }
}

/// Frame rates as editors write them: whole numbers bare, the broadcast rates
/// to as many decimals as they need and no more (29.97, not 29.970).
fn fmt_fps(fps: f64) -> String {
    if (fps - fps.round()).abs() < 0.001 {
        format!("{}", fps.round() as i64)
    } else {
        let s = format!("{fps:.3}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

fn compute_lane_height(tracks_area_h: f32, n_tracks: usize) -> f32 {
    let n = n_tracks.max(1) as f32;
    let gaps = (n - 1.0).max(0.0) * TRACK_LANE_GAP;
    let avail = (tracks_area_h * TRACK_LANE_FILL - gaps).max(0.0);
    (avail / n)
        .clamp(TRACK_LANE_MIN_H, TRACK_LANE_MAX_H)
        .round()
}

impl TimelineLayout {
    fn cursor_to_t(&self, cursor_x: f32) -> f64 {
        let ratio = ((cursor_x - self.clips_x) / self.clips_w).clamp(0.0, 1.0) as f64;
        ratio * self.duration
    }

    fn t_to_x(&self, t: f64) -> f32 {
        self.clips_x + (t / self.duration) as f32 * self.clips_w
    }
}

/// Shorten `text` so it fits within `max_w` when rendered at `size_px`,
/// appending an ellipsis if truncation happened. Returns the original string
/// when it already fits, so the common case stays zero-allocation at the call
/// site (the caller passes a `&str` either way).
fn truncate_to_width(text: &TextRenderer, s: &str, size_px: f32, max_w: f32) -> String {
    if text.measure_width(s, size_px) <= max_w {
        return s.to_string();
    }
    let ellipsis = "…";
    let ell_w = text.measure_width(ellipsis, size_px);
    if ell_w > max_w {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0.0;
    for ch in s.chars() {
        let ch_w = text.measure_width(&ch.to_string(), size_px);
        if used + ch_w + ell_w > max_w {
            break;
        }
        out.push(ch);
        used += ch_w;
    }
    out.push_str(ellipsis);
    out
}

fn format_timecode(t: f64) -> String {
    let total_ms = (t.max(0.0) * 1000.0) as u64;
    let ms = total_ms % 1000;
    let sec = total_ms / 1000;
    let m = sec / 60;
    let s = sec % 60;
    format!("{:02}:{:02}.{:03}", m, s, ms)
}

/// V1, V2, A1, A2 — the model supports arbitrary mixes; this is just a sensible
/// starting point so a blank session shows multiple lanes immediately.
fn default_tracks() -> Vec<Track> {
    vec![
        Track::new(TrackKind::Video),
        Track::new(TrackKind::Video),
        Track::new(TrackKind::Audio),
        Track::new(TrackKind::Audio),
    ]
}

fn import_source(
    media: &mut MediaPool,
    path: &str,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    quads: &QuadRenderer,
) {
    if let Err(e) = media.add(path, device, queue, quads) {
        log::error!("failed to load {path}: {e}");
    }
}

struct State {
    instance: wgpu::Instance,
    window: Arc<Window>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// Surface size in physical pixels. Only the swapchain and the projection
    /// want this — everything else works through [`State::logical_size`].
    size: winit::dpi::PhysicalSize<u32>,
    /// Physical pixels per logical point, i.e. 2.0 on a Retina display.
    ///
    /// Every layout constant in this file is in points, so the UI keeps the
    /// same *physical* size across displays instead of halving on a HiDPI one,
    /// while a bigger window shows more timeline rather than a bigger toolbar.
    /// Nothing multiplies by this directly: the projection is fed a viewport in
    /// points, which makes the conversion the GPU's job. A user zoom
    /// preference, when it arrives, folds in here as a second factor.
    scale: f32,
    surface: wgpu::Surface<'static>,
    surface_format: wgpu::TextureFormat,
    quads: QuadRenderer,
    text: TextRenderer,
    media: MediaPool,
    timeline: Timeline,
    audio: AudioEngine,
    /// In logical points, matching the rects it is tested against.
    cursor: [f32; 2],
    /// Fraction of the window height that sits above the timeline, and fraction
    /// of the width given to the media pool. Read through [`State::timeline_top`]
    /// and [`State::media_pool_w`], which apply the panel minimums.
    split_top_bottom: f32,
    split_pool_preview: f32,
    /// Canvas format, each dimension either pinned or following the footage.
    /// Read together through [`State::canvas`].
    canvas_res: Setting<(u32, u32)>,
    canvas_fps: Setting<f64>,
    project_menu_open: bool,
    /// Popup geometry, filled in while drawing and consumed by the next click.
    /// Storing what was drawn — rather than recomputing the layout to hit-test
    /// it — is what keeps the two from disagreeing about where a row is.
    project_menu_rect: Rect,
    project_menu_items: Vec<(Rect, ProjectChoice)>,
    /// Last icon handed to the window, so a hover that doesn't change the
    /// cursor doesn't re-set it every frame.
    cursor_icon: CursorIcon,
    drag: DragMode,
    last_playing_source: Option<SourceId>,
    /// Prev-edit, prev-frame, play/pause, next-frame, next-edit — left to
    /// right, so the outer buttons are the coarser jumps.
    transport: [Rect; 5],
    timeline_split_btn: Rect,
    timeline_undo_btn: Rect,
    timeline_redo_btn: Rect,
    timeline_snap_btn: Rect,
    timeline_delete_btn: Rect,
    /// Clip id, not a position — see [`Clip::id`]. A selection whose clip has
    /// been deleted simply resolves to nothing, and comes back if an undo
    /// restores the clip.
    selected: Option<u32>,
    /// Right-aligned in the toolbar row, well clear of the edit buttons: this
    /// one produces a file rather than changing the timeline.
    timeline_export_btn: Rect,
    timeline_project_btn: Rect,
    /// The project-file pair, left of the gear. Grouped at the right end with
    /// the settings and export buttons because all four concern the project
    /// rather than the timeline, which is what the left cluster edits.
    timeline_open_btn: Rect,
    timeline_save_btn: Rect,
    /// The render in flight, if any. Only one at a time — the button greys out
    /// while it runs, and clicking it again cancels.
    export: Option<ExportJob>,
    /// Outcome of the last thing worth reporting — a render, a save, a failed
    /// open — shown until it ages out.
    status: Option<(String, [f32; 4], Instant)>,
    /// Magnetic snapping while dragging. Toggleable because there is no
    /// timeline zoom yet: on a long timeline the pixel threshold covers a wide
    /// time window, and without an escape hatch a clip could not be parked
    /// near a neighbour without latching onto it.
    snap_enabled: bool,
    pool_open_btn: Rect,
    modifiers: ModifiersState,
    undo_stack: Vec<EditSnapshot>,
    redo_stack: Vec<EditSnapshot>,
    /// State captured before the in-flight edit. Held here rather than pushed
    /// immediately so a drag that fires every mouse-move still collapses into
    /// a single undo step, and so a no-op edit can be discarded.
    pending_edit: Option<EditSnapshot>,
    /// Open `begin_edit` calls. Only the outermost pair produces an undo step,
    /// so a batch operation can wrap self-contained edits and still read as
    /// one Ctrl+Z.
    edit_depth: u32,
    /// Where Ctrl+S writes without asking. `None` until the project has been
    /// saved once or opened from disk.
    project_path: Option<PathBuf>,
    /// Whether anything has changed since the last save. Drives the dot in the
    /// title bar and the prompt on close.
    dirty: bool,
    /// Last string handed to the window manager, so the title is only re-set
    /// when it actually changes rather than every frame.
    title_shown: String,
}

impl State {
    async fn new(display: OwnedDisplayHandle, window: Arc<Window>) -> State {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle(
            Box::new(display),
        ));
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .unwrap();
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default())
            .await
            .unwrap();

        let size = window.inner_size();

        let surface = instance.create_surface(window.clone()).unwrap();
        let cap = surface.get_capabilities(&adapter);
        let surface_format = cap.formats[0];

        let quads = QuadRenderer::new(&device, &queue, surface_format.add_srgb_suffix());
        let text = TextRenderer::new(&device, &quads);

        let mut timeline = Timeline::new();
        timeline.tracks = default_tracks();

        // A project on the command line replaces the arguments-as-media path
        // entirely, so `videoEditor edit.vedit` opens the edit rather than
        // trying to decode it. Deferred until `State` exists, since loading
        // one is a method on it.
        let args: Vec<String> = std::env::args().skip(1).collect();
        let project_arg = args
            .iter()
            .find(|a| Path::new(a).extension().is_some_and(|e| e == project::EXTENSION))
            .cloned();

        let mut media = MediaPool::new();
        if project_arg.is_none() {
            for path in &args {
                import_source(&mut media, path, &device, &queue, &quads);
            }
        }

        let scale = window.scale_factor() as f32;

        let mut state = State {
            instance,
            window,
            device,
            queue,
            size,
            scale,
            surface,
            surface_format,
            quads,
            text,
            media,
            timeline,
            audio: AudioEngine::new(),
            cursor: [0.0, 0.0],
            split_top_bottom: TOP_BOTTOM_SPLIT,
            split_pool_preview: MEDIA_PREVIEW_SPLIT,
            canvas_res: Setting::Auto,
            canvas_fps: Setting::Auto,
            project_menu_open: false,
            project_menu_rect: Rect::default(),
            project_menu_items: Vec::new(),
            cursor_icon: CursorIcon::Default,
            drag: DragMode::None,
            last_playing_source: None,
            transport: [Rect::default(); 5],
            timeline_split_btn: Rect::default(),
            timeline_undo_btn: Rect::default(),
            timeline_redo_btn: Rect::default(),
            timeline_snap_btn: Rect::default(),
            timeline_delete_btn: Rect::default(),
            selected: None,
            timeline_export_btn: Rect::default(),
            timeline_project_btn: Rect::default(),
            timeline_open_btn: Rect::default(),
            timeline_save_btn: Rect::default(),
            export: None,
            status: None,
            snap_enabled: true,
            pool_open_btn: Rect::default(),
            modifiers: ModifiersState::empty(),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            pending_edit: None,
            edit_depth: 0,
            project_path: None,
            dirty: false,
            title_shown: String::new(),
        };

        state.configure_surface();
        state.set_scale(scale);
        if let Some(path) = project_arg {
            state.load_project(Path::new(&path));
        }
        state.update_title();

        state
    }

    fn get_window(&self) -> &Window {
        &self.window
    }

    fn configure_surface(&self) {
        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: self.surface_format,
            view_formats: vec![self.surface_format.add_srgb_suffix()],
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            width: self.size.width,
            height: self.size.height,
            desired_maximum_frame_latency: 2,
            present_mode: wgpu::PresentMode::AutoVsync,
        };
        self.surface.configure(&self.device, &surface_config);
    }

    fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        self.size = new_size;
        self.configure_surface();
    }

    /// Adopt a new device pixel ratio, e.g. after the window is dragged onto a
    /// display with a different one. The surface itself needs no work here:
    /// winit follows a scale change with a `Resized` carrying the new physical
    /// size, which [`State::resize`] handles.
    fn set_scale(&mut self, scale: f32) {
        self.scale = scale;
        self.text.set_scale(scale);
    }

    /// Window size in logical points — the coordinate space all layout, hit
    /// testing and drawing below is written in. See [`State::scale`].
    fn logical_size(&self) -> [f32; 2] {
        [
            self.size.width as f32 / self.scale,
            self.size.height as f32 / self.scale,
        ]
    }

    /// Y of the divider between the timeline and the panels above it.
    fn timeline_top(&self) -> f32 {
        let h = self.logical_size()[1];
        resolve_split(self.split_top_bottom, h, TOP_MIN_H, TIMELINE_MIN_H)
    }

    /// Width of the media pool, i.e. X of the divider between it and the preview.
    fn media_pool_w(&self) -> f32 {
        let w = self.logical_size()[0];
        resolve_split(self.split_pool_preview, w, POOL_MIN_W, PREVIEW_MIN_W)
    }

    /// Which divider, if any, the cursor is close enough to grab.
    ///
    /// The horizontal one is tested first and spans the full width, so at the
    /// T-junction where the two meet it wins. Either answer is defensible
    /// there; what matters is that the hover highlight and the press agree,
    /// which they do by both coming through here.
    fn splitter_at(&self, [cx, cy]: [f32; 2]) -> Option<Splitter> {
        let top = self.timeline_top();
        if (cy - top).abs() <= SPLITTER_GRAB_PX {
            return Some(Splitter::TopBottom);
        }
        if cy < top && (cx - self.media_pool_w()).abs() <= SPLITTER_GRAB_PX {
            return Some(Splitter::PoolPreview);
        }
        None
    }

    /// Point the cursor at whichever divider it is over or dragging. Dragging
    /// takes priority: once a splitter has been grabbed the cursor keeps its
    /// resize shape even as it runs past the panel's minimum and off the line.
    fn update_cursor_icon(&mut self) {
        let splitter = match self.drag {
            DragMode::Splitter(s) => Some(s),
            DragMode::None => self.splitter_at(self.cursor),
            // Mid-gesture on something else: leave the pointer alone rather
            // than flickering to a resize arrow while a clip is dragged past.
            _ => None,
        };
        let icon = match splitter {
            Some(Splitter::TopBottom) => CursorIcon::RowResize,
            Some(Splitter::PoolPreview) => CursorIcon::ColResize,
            None => CursorIcon::Default,
        };
        if icon != self.cursor_icon {
            self.window.set_cursor(icon);
            self.cursor_icon = icon;
        }
    }

    fn timeline_layout(&self) -> TimelineLayout {
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

    fn n_video_tracks(&self) -> usize {
        self.timeline
            .tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Video)
            .count()
    }

    fn pool_hit(&self, cursor_x: f32, cursor_y: f32) -> Option<SourceId> {
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

    fn pool_close_hit(&self, cursor_x: f32, cursor_y: f32) -> Option<SourceId> {
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

    fn remove_source(&mut self, id: SourceId) {
        self.begin_edit();
        self.media.remove(id);
        self.timeline.remove_source(id);
        if self.last_playing_source == Some(id) {
            self.last_playing_source = None;
        }
        self.commit_edit();
    }

    fn edit_snapshot(&self) -> EditSnapshot {
        EditSnapshot {
            timeline: self.timeline.snapshot(),
            pool_order: self.media.ids().to_vec(),
        }
    }

    /// Open an undoable edit. Nests: only the outermost begin/commit pair
    /// yields a step, so a batch (multi-file import) can wrap operations that
    /// each manage their own edit. Must be paired with `commit_edit`.
    fn begin_edit(&mut self) {
        if self.edit_depth == 0 {
            self.pending_edit = Some(self.edit_snapshot());
        }
        self.edit_depth += 1;
    }

    /// Close the edit opened by `begin_edit`. Edits that changed nothing —
    /// a click that never dragged, a split landing on a clip boundary — are
    /// dropped so Ctrl+Z never appears to do nothing.
    fn commit_edit(&mut self) {
        self.edit_depth = self.edit_depth.saturating_sub(1);
        if self.edit_depth > 0 {
            return;
        }
        let Some(before) = self.pending_edit.take() else {
            return;
        };
        if before == self.edit_snapshot() {
            return;
        }
        if self.undo_stack.len() >= UNDO_LIMIT {
            self.undo_stack.remove(0);
        }
        self.undo_stack.push(before);
        // A fresh edit invalidates the redo branch.
        self.redo_stack.clear();
        // Only reached when the edit actually changed something, which is
        // exactly when the saved file goes stale.
        self.dirty = true;
    }

    fn undo(&mut self) {
        let Some(prev) = self.undo_stack.pop() else {
            return;
        };
        let current = self.edit_snapshot();
        self.apply_snapshot(&prev);
        self.redo_stack.push(current);
        // Undoing back to exactly the saved state still counts as dirty. The
        // alternative is comparing against a snapshot taken at save time, which
        // is more bookkeeping than an over-eager prompt is worth.
        self.dirty = true;
    }

    fn redo(&mut self) {
        let Some(next) = self.redo_stack.pop() else {
            return;
        };
        let current = self.edit_snapshot();
        self.apply_snapshot(&next);
        self.undo_stack.push(current);
        self.dirty = true;
    }

    fn apply_snapshot(&mut self, snap: &EditSnapshot) {
        // An in-flight drag holds (track, idx) coordinates that the restored
        // timeline may no longer have — cancel it rather than index a clip
        // that undo just deleted. Dropping the pending snapshot with it means
        // the interrupted drag leaves no half-finished step behind.
        self.drag = DragMode::None;
        self.pending_edit = None;
        self.edit_depth = 0;
        self.timeline.restore(&snap.timeline);
        self.media.set_order(&snap.pool_order);
        // Recomputed from the timeline on the next render.
        self.last_playing_source = None;
        // Undoing a drop or a trim can shorten the timeline out from under
        // the playhead; pull it back in bounds.
        let duration = self.timeline.duration();
        if self.audio.position() > duration {
            self.audio.set_position(duration);
        }
    }

    /// Locate the visual track lane under `cursor_y`. Returns the track index
    /// whose lane *center* is nearest — gaps snap to the nearer lane so drops
    /// near a boundary feel forgiving.
    fn track_at_y(&self, cursor_y: f32, layout: &TimelineLayout) -> Option<usize> {
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

    fn timeline_hit(&self, cursor_x: f32, cursor_y: f32) -> TimelineHit {
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

    fn begin_drag(&mut self) {
        // A press with an edit still open means the matching release never
        // arrived (focus lost mid-drag). Close it here — otherwise the depth
        // counter leaks and every later edit nests inside it, silently
        // stalling history for the rest of the session.
        if self.edit_depth > 0 {
            self.edit_depth = 1;
            self.commit_edit();
        }
        let [cx, cy] = self.cursor;
        // The popup is drawn over everything, so it is tested before everything
        // — including the splitters, which it can overlap.
        if self.project_menu_open {
            if let Some((_, choice)) = self
                .project_menu_items
                .iter()
                .find(|(rect, _)| rect.contains([cx, cy]))
            {
                let choice = *choice;
                self.apply_project_choice(choice);
                return;
            }
            // A click anywhere else dismisses it, and is swallowed rather than
            // falling through: a press meant to close a popup should not also
            // scrub the timeline or clear the selection underneath it.
            self.project_menu_open = false;
            return;
        }
        if self.timeline_project_btn.contains([cx, cy]) {
            self.project_menu_open = true;
            return;
        }
        // Before every button and panel: a divider sits on top of the panels it
        // separates, and its grab band overlaps them by design. Nothing else
        // lives within a few points of one, so this costs the rest nothing.
        if let Some(splitter) = self.splitter_at([cx, cy]) {
            self.drag = DragMode::Splitter(splitter);
            return;
        }
        if self.transport[0].contains([cx, cy]) {
            self.goto_edit_point(false);
            return;
        }
        if self.transport[1].contains([cx, cy]) {
            self.step_frame(-1.0);
            return;
        }
        if self.transport[2].contains([cx, cy]) {
            self.toggle_playback();
            return;
        }
        if self.transport[3].contains([cx, cy]) {
            self.step_frame(1.0);
            return;
        }
        if self.transport[4].contains([cx, cy]) {
            self.goto_edit_point(true);
            return;
        }
        if self.timeline_split_btn.contains([cx, cy]) {
            self.split_at_playhead();
            return;
        }
        // Consume the click even with an empty stack — undo() no-ops, and
        // falling through would start a scrub under the toolbar.
        if self.timeline_undo_btn.contains([cx, cy]) {
            self.undo();
            return;
        }
        if self.timeline_redo_btn.contains([cx, cy]) {
            self.redo();
            return;
        }
        if self.timeline_delete_btn.contains([cx, cy]) {
            self.delete_selected();
            return;
        }
        if self.timeline_snap_btn.contains([cx, cy]) {
            self.snap_enabled = !self.snap_enabled;
            return;
        }
        if self.timeline_export_btn.contains([cx, cy]) {
            self.start_export();
            return;
        }
        if self.timeline_open_btn.contains([cx, cy]) {
            self.open_project();
            return;
        }
        // Consumed even when the project is clean and the button is greyed, for
        // the same reason as undo: falling through would scrub the timeline
        // underneath the toolbar.
        if self.timeline_save_btn.contains([cx, cy]) {
            if self.dirty {
                self.save_project(false);
            }
            return;
        }
        if self.pool_open_btn.contains([cx, cy]) {
            self.open_file_picker();
            return;
        }
        if let Some(id) = self.pool_close_hit(cx, cy) {
            self.remove_source(id);
            return;
        }
        if let Some(source) = self.pool_hit(cx, cy) {
            self.drag = DragMode::PoolDrag { source };
            return;
        }
        // Clip drags mutate continuously; snapshot once here so the whole
        // gesture collapses to one undo step (closed in `end_drag`).
        //
        // Touching a clip at all selects it — including by its trim handles,
        // since a press there is still a statement about which clip you mean.
        // Pressing bare lane or ruler clears, so there is always a way to
        // deselect without a modifier.
        match self.timeline_hit(cx, cy) {
            TimelineHit::ClipTrimLeft { track, idx } => {
                self.select_clip_at(track, idx);
                self.begin_edit();
                self.drag = DragMode::ClipTrimLeft { track, idx };
            }
            TimelineHit::ClipTrimRight { track, idx } => {
                self.select_clip_at(track, idx);
                self.begin_edit();
                self.drag = DragMode::ClipTrimRight { track, idx };
            }
            TimelineHit::ClipBody { track, idx, grab_t_offset } => {
                self.select_clip_at(track, idx);
                self.begin_edit();
                self.drag = DragMode::ClipMove { track, idx, grab_t_offset };
            }
            TimelineHit::Lane { .. } | TimelineHit::Ruler => {
                self.selected = None;
                self.drag = DragMode::Scrub;
                self.apply_scrub();
            }
            TimelineHit::None => {}
        }
    }

    fn select_clip_at(&mut self, track: usize, idx: usize) {
        self.selected = Some(self.timeline.tracks[track].clips[idx].id);
    }

    fn update_drag(&mut self) {
        match self.drag {
            DragMode::None | DragMode::PoolDrag { .. } => {}
            DragMode::Scrub => self.apply_scrub(),
            // Store the clamped position rather than the raw cursor. Past a
            // minimum the divider stops either way, but re-clamping on write
            // means it is back under the cursor the instant you drag back,
            // instead of trailing by however far you overshot.
            // The `max` keeps a zero-sized window (minimized, on some platforms)
            // from storing a NaN fraction, which would never wash back out.
            DragMode::Splitter(Splitter::TopBottom) => {
                let h = self.logical_size()[1].max(1.0);
                let y = resolve_split(self.cursor[1] / h, h, TOP_MIN_H, TIMELINE_MIN_H);
                self.split_top_bottom = y / h;
            }
            DragMode::Splitter(Splitter::PoolPreview) => {
                let w = self.logical_size()[0].max(1.0);
                let x = resolve_split(self.cursor[0] / w, w, POOL_MIN_W, PREVIEW_MIN_W);
                self.split_pool_preview = x / w;
            }
            DragMode::ClipMove { track, idx, grab_t_offset } => {
                let layout = self.timeline_layout();
                // Allow vertical drag: if the cursor is over a different
                // same-kind track, relocate the clip there before nudging x.
                // Cross-kind moves (V↔A) are blocked — a video clip on an
                // audio lane has no meaningful playback behavior yet. Linked
                // siblings stay on their own tracks; only the dragged clip
                // changes track membership.
                let (track, idx) =
                    if let Some(hover) = self.track_at_y(self.cursor[1], &layout) {
                        let src_kind = self.timeline.tracks[track].kind;
                        let dst_kind = self.timeline.tracks[hover].kind;
                        if hover != track && src_kind == dst_kind {
                            let clip = self.timeline.tracks[track].clips.remove(idx);
                            let new_idx = self.timeline.tracks[hover].clips.len();
                            self.timeline.tracks[hover].clips.push(clip);
                            self.drag = DragMode::ClipMove {
                                track: hover,
                                idx: new_idx,
                                grab_t_offset,
                            };
                            (hover, new_idx)
                        } else {
                            (track, idx)
                        }
                    } else {
                        (track, idx)
                    };
                let cursor_t = layout.cursor_to_t(self.cursor[0]);
                let current_start =
                    self.timeline.tracks[track].clips[idx].timeline_start;
                let desired_delta = (cursor_t - grab_t_offset) - current_start;
                self.apply_move_delta(track, idx, desired_delta);
            }
            DragMode::ClipTrimLeft { track, idx } => {
                let layout = self.timeline_layout();
                let cursor_t = layout.cursor_to_t(self.cursor[0]);
                let current_start =
                    self.timeline.tracks[track].clips[idx].timeline_start;
                let desired_delta = cursor_t - current_start;
                self.apply_trim_left_delta(track, idx, desired_delta);
            }
            DragMode::ClipTrimRight { track, idx } => {
                let layout = self.timeline_layout();
                let cursor_t = layout.cursor_to_t(self.cursor[0]);
                let current_end =
                    self.timeline.tracks[track].clips[idx].timeline_end();
                let desired_delta = cursor_t - current_end;
                self.apply_trim_right_delta(track, idx, desired_delta);
            }
        }
    }

    /// Indices of every clip linked to `(track, idx)`, including itself.
    /// Unlinked clips return just their own position.
    fn linked_siblings(&self, track: usize, idx: usize) -> Vec<(usize, usize)> {
        let link = self.timeline.tracks[track].clips[idx].link;
        let Some(link_id) = link else {
            return vec![(track, idx)];
        };
        let mut v = Vec::new();
        for (ti, tr) in self.timeline.tracks.iter().enumerate() {
            for (ci, c) in tr.clips.iter().enumerate() {
                if c.link == Some(link_id) {
                    v.push((ti, ci));
                }
            }
        }
        v
    }

    /// Times a dragged edge can latch onto: every other clip's edges, the
    /// timeline start, and the playhead. Targets are collected across all
    /// tracks so a clip can be lined up with one on another lane — that's how
    /// you align a cutaway to an edit below it.
    ///
    /// `exclude` is the moving group. A linked pair travels as a unit, so
    /// leaving its own edges in the target set would pin it in place.
    fn snap_targets(&self, exclude: &[(usize, usize)]) -> Vec<f64> {
        let mut pts = vec![0.0, self.audio.position()];
        for (ti, tr) in self.timeline.tracks.iter().enumerate() {
            for (ci, c) in tr.clips.iter().enumerate() {
                if exclude.contains(&(ti, ci)) {
                    continue;
                }
                pts.push(c.timeline_start);
                pts.push(c.timeline_end());
            }
        }
        pts
    }

    /// Nudge `delta` so the closest dragged edge lands exactly on a snap
    /// target. Returns `delta` untouched when nothing is in range or snapping
    /// is off.
    fn snap_move_delta(&self, siblings: &[(usize, usize)], delta: f64) -> f64 {
        if !self.snap_enabled {
            return delta;
        }
        let layout = self.timeline_layout();
        // The threshold is authored in pixels, so convert with the same
        // seconds-per-pixel mapping the drag itself uses.
        let px_per_sec = layout.clips_w as f64 / layout.duration;
        if !px_per_sec.is_finite() || px_per_sec <= 0.0 {
            return delta;
        }
        let edges: Vec<f64> = siblings
            .iter()
            .flat_map(|&(ti, ci)| {
                let c = &self.timeline.tracks[ti].clips[ci];
                [c.timeline_start + delta, c.timeline_end() + delta]
            })
            .collect();
        let targets = self.snap_targets(siblings);
        delta + nearest_snap(&edges, &targets, SNAP_PX as f64 / px_per_sec).unwrap_or(0.0)
    }

    fn apply_move_delta(&mut self, track: usize, idx: usize, desired_delta: f64) {
        let siblings = self.linked_siblings(track, idx);
        // Clamp so the earliest-starting sibling doesn't go negative —
        // applying the same delta everywhere preserves the sync offset.
        let min_start = siblings
            .iter()
            .map(|&(ti, ci)| self.timeline.tracks[ti].clips[ci].timeline_start)
            .fold(f64::INFINITY, f64::min);
        // Snap after the zero-clamp so the latch is computed against where the
        // clip can actually go, then re-clamp as a backstop.
        let delta = self
            .snap_move_delta(&siblings, desired_delta.max(-min_start))
            .max(-min_start);
        for (ti, ci) in siblings {
            self.timeline.tracks[ti].clips[ci].timeline_start += delta;
        }
    }

    fn apply_trim_left_delta(&mut self, track: usize, idx: usize, desired_delta: f64) {
        let siblings = self.linked_siblings(track, idx);
        // Delta bounds: the same delta shifts every sibling's source_in and
        // timeline_start, so the allowed range is the intersection of each
        // sibling's own limits.
        let mut min_delta = f64::NEG_INFINITY;
        let mut max_delta = f64::INFINITY;
        for &(ti, ci) in &siblings {
            let c = &self.timeline.tracks[ti].clips[ci];
            min_delta = min_delta.max(-c.source_in);
            min_delta = min_delta.max(-c.timeline_start);
            max_delta = max_delta.min(c.duration() - MIN_CLIP_DURATION);
        }
        let delta = desired_delta.clamp(min_delta, max_delta);
        for (ti, ci) in siblings {
            let c = &mut self.timeline.tracks[ti].clips[ci];
            c.source_in += delta;
            c.timeline_start += delta;
        }
    }

    fn apply_trim_right_delta(&mut self, track: usize, idx: usize, desired_delta: f64) {
        let siblings = self.linked_siblings(track, idx);
        let mut min_delta = f64::NEG_INFINITY;
        let mut max_delta = f64::INFINITY;
        for &(ti, ci) in &siblings {
            let c = &self.timeline.tracks[ti].clips[ci];
            // Can't shrink below the minimum clip duration.
            min_delta = min_delta.max(MIN_CLIP_DURATION - c.duration());
            // Can't extend past the source's end — cap per-track since video
            // and audio streams of the same source can have different lengths.
            let src_dur = match self.timeline.tracks[ti].kind {
                TrackKind::Video => self.media.duration(c.source),
                TrackKind::Audio => self.media.audio_duration(c.source).unwrap_or(c.source_out),
            };
            max_delta = max_delta.min(src_dur - c.source_out);
        }
        let delta = desired_delta.clamp(min_delta, max_delta);
        for (ti, ci) in siblings {
            let c = &mut self.timeline.tracks[ti].clips[ci];
            c.source_out += delta;
        }
    }

    fn end_drag(&mut self) {
        if let DragMode::PoolDrag { source } = self.drag {
            let [cx, cy] = self.cursor;
            let layout = self.timeline_layout();
            if let Some(track_idx) = self.track_at_y(cy, &layout) {
                self.begin_edit();
                let drop_t = layout.cursor_to_t(cx).max(0.0);
                let kind = self.timeline.tracks[track_idx].kind;
                match kind {
                    TrackKind::Video => {
                        let dur = self.media.duration(source);
                        // Decide up front whether we're auto-pairing audio —
                        // only then do we need a link id, and both sides must
                        // use the same one.
                        let audio_target = self
                            .media
                            .has_audio(source)
                            .then(|| {
                                self.timeline
                                    .tracks
                                    .iter()
                                    .position(|t| t.kind == TrackKind::Audio)
                            })
                            .flatten();
                        let link = audio_target.map(|_| self.timeline.new_link_id());
                        let id = self.timeline.new_clip_id();
                        self.timeline.tracks[track_idx].clips.push(Clip {
                            id,
                            source,
                            source_in: 0.0,
                            source_out: dur,
                            timeline_start: drop_t,
                            link,
                        });
                        if let Some(audio_idx) = audio_target {
                            let adur = self.media.audio_duration(source).unwrap_or(dur);
                            let id = self.timeline.new_clip_id();
                            self.timeline.tracks[audio_idx].clips.push(Clip {
                                id,
                                source,
                                source_in: 0.0,
                                source_out: adur,
                                timeline_start: drop_t,
                                link,
                            });
                        }
                    }
                    TrackKind::Audio => {
                        if let Some(adur) = self.media.audio_duration(source) {
                            let id = self.timeline.new_clip_id();
                            self.timeline.tracks[track_idx].clips.push(Clip {
                                id,
                                source,
                                source_in: 0.0,
                                source_out: adur,
                                timeline_start: drop_t,
                                link: None,
                            });
                        }
                        // Dropping a video-only source on an audio lane is a
                        // no-op — there's nothing to play back there.
                    }
                }
            }
        }
        // Closes whichever edit `begin_drag` or the pool drop opened; a no-op
        // gesture (click without move, drop that landed nowhere) discards it.
        self.commit_edit();
        self.drag = DragMode::None;
    }

    fn apply_scrub(&mut self) {
        let duration = self.timeline.duration();
        if duration <= 0.0 {
            return;
        }
        let layout = self.timeline_layout();
        let t = layout.cursor_to_t(self.cursor[0]);
        // Audio engine owns the playhead — setting position also flushes any
        // pre-mixed samples so the next tick refills from the new time,
        // keeping scrub snappy instead of dragging 150ms of stale audio.
        self.audio.set_position(t);
    }

    /// Frame stepping follows the canvas, not the clip under the playhead: a
    /// frame is a unit of the project, and on a mixed-rate timeline stepping by
    /// whatever happens to be underneath made a "frame" change length partway
    /// along the sequence.
    fn current_fps(&self) -> f64 {
        self.canvas().fps
    }

    fn step_frame(&mut self, dir: f64) {
        if self.audio.playing() {
            self.audio.set_playing(false);
        }
        let fps = self.current_fps().max(1.0);
        let dt = dir / fps;
        let mut new_t = (self.audio.position() + dt).max(0.0);
        let duration = self.timeline.duration();
        if duration > 0.0 {
            new_t = new_t.min(duration);
        }
        self.audio.set_position(new_t);
    }

    /// Jump to the clip boundary before/after the playhead. Pauses like
    /// `step_frame` does — these sit in the same row of navigation buttons and
    /// splitting the pause behavior between them would be arbitrary.
    fn goto_edit_point(&mut self, forward: bool) {
        let t = self.audio.position();
        let target = if forward {
            self.timeline.next_edit_point(t)
        } else {
            self.timeline.prev_edit_point(t)
        };
        let Some(target) = target else {
            return;
        };
        if self.audio.playing() {
            self.audio.set_playing(false);
        }
        self.audio.set_position(target);
    }

    fn split_at_playhead(&mut self) {
        let t = self.audio.position();
        self.begin_edit();
        self.timeline.split_at(t);
        self.commit_edit();
    }

    /// Whether [`State::delete_selected`] would actually remove anything.
    fn has_selection(&self) -> bool {
        self.selected
            .is_some_and(|id| self.timeline.find(id).is_some())
    }

    /// Remove the selected clip, taking its linked siblings with it. Linked
    /// A/V travels as a unit everywhere else — move, trim, split — so deleting
    /// only half of a pair would be the odd one out.
    fn delete_selected(&mut self) {
        let Some((track, idx)) = self.selected.and_then(|id| self.timeline.find(id)) else {
            return;
        };
        // Resolve to ids before touching anything: every removal shifts the
        // indices of the clips after it, so a list of positions goes stale the
        // moment the first one is used.
        let doomed: Vec<u32> = self
            .linked_siblings(track, idx)
            .into_iter()
            .map(|(ti, ci)| self.timeline.tracks[ti].clips[ci].id)
            .collect();
        self.begin_edit();
        for t in &mut self.timeline.tracks {
            t.clips.retain(|c| !doomed.contains(&c.id));
        }
        self.commit_edit();
        self.selected = None;
    }

    fn toggle_playback(&mut self) {
        self.audio.toggle();
    }

    /// Undoable: an import only adds a pool row, so undo hides it again.
    /// Callers importing a batch should wrap the whole batch in their own
    /// begin/commit pair to get one step for the batch.
    fn import_file(&mut self, path: &str) {
        self.begin_edit();
        import_source(
            &mut self.media,
            path,
            &self.device,
            &self.queue,
            &self.quads,
        );
        self.commit_edit();
    }

    fn open_file_picker(&mut self) {
        // Blocking dialog is fine here: a single-user editor pausing the event
        // loop while the OS picker is up is the expected behavior.
        let Some(paths) = rfd::FileDialog::new()
            .add_filter("video", &["mp4", "mov", "mkv", "webm", "avi", "m4v"])
            .pick_files()
        else {
            return;
        };
        // One picker interaction is one undo step, however many files it
        // brought in — the per-file edits nest inside this one.
        self.begin_edit();
        for path in paths {
            if let Some(p) = path.to_str() {
                self.import_file(p);
            }
        }
        self.commit_edit();
    }

    /// Picture size and rate for a render, taken from the timeline's first
    /// video clip — earliest start, lowest track on a tie. Matching one clip
    /// rather than, say, the largest source keeps the common single-camera
    /// timeline a straight passthrough; anything else letterboxes into it.
    /// The video clip whose format `Setting::Auto` follows: earliest on the
    /// timeline, ties broken by the lower track.
    fn reference_video_source(&self) -> Option<SourceId> {
        let mut reference: Option<(f64, usize, SourceId)> = None;
        for (track_idx, track) in self.timeline.tracks.iter().enumerate() {
            if track.kind != TrackKind::Video {
                continue;
            }
            for clip in &track.clips {
                let better = reference.is_none_or(|(start, ti, _)| {
                    (clip.timeline_start, track_idx) < (start, ti)
                });
                if better {
                    reference = Some((clip.timeline_start, track_idx, clip.source));
                }
            }
        }
        reference.map(|(_, _, source)| source)
    }

    /// The project canvas: pinned dimensions where the user set them, the
    /// reference clip's where they didn't.
    ///
    /// Single source of truth for the preview, the export and frame stepping.
    /// They used to derive their formats separately, so on a mixed-rate
    /// timeline stepping "one frame" and exporting one frame meant different
    /// durations.
    fn canvas(&self) -> Canvas {
        let reference = self
            .reference_video_source()
            .and_then(|source| self.media.get(source));
        let (width, height) = match self.canvas_res {
            Setting::Fixed(size) => size,
            Setting::Auto => reference
                .map(|src| (src.stream.width(), src.stream.height()))
                .unwrap_or(EXPORT_FALLBACK_SIZE),
        };
        let fps = match self.canvas_fps {
            Setting::Fixed(fps) => fps,
            Setting::Auto => reference.map(|src| src.stream.frame_rate()).unwrap_or(0.0),
        };
        Canvas {
            // H.264 in YUV420P cannot represent an odd dimension, and a source
            // with one would otherwise fail deep inside the encoder. Applied
            // here rather than at export so the preview frames exactly what
            // will be written.
            width: (width & !1).max(2),
            height: (height & !1).max(2),
            fps: if fps > 0.0 { fps } else { EXPORT_FALLBACK_FPS },
        }
    }

    /// `None` renders audio-only. A canvas exists either way, but with no video
    /// on the timeline there is nothing to draw on it, and encoding a silent
    /// black picture is not what "export" means here.
    fn export_video_spec(&self) -> Option<VideoSpec> {
        self.reference_video_source()?;
        let canvas = self.canvas();
        Some(VideoSpec {
            width: canvas.width,
            height: canvas.height,
            fps: canvas.fps,
        })
    }

    /// Draw the project settings popup and record where its rows landed.
    ///
    /// Anchored below the gear when there's room and above it when there isn't,
    /// then pulled inside the window — the toolbar sits partway down the
    /// window, so which side has space depends on where the user has dragged
    /// the timeline splitter.
    fn draw_project_menu(&mut self, anchor: Rect, win_w: f32, win_h: f32) {
        self.project_menu_items.clear();

        let fps_grid_w = MENU_FPS_COL_W * 3.0 + MENU_FPS_COL_GAP * 2.0;
        let body_w = MENU_RES_W.max(fps_grid_w);
        let menu_w = body_w + MENU_PAD * 2.0;

        let header_h = (self.text.ascent(MENU_HEADER_SIZE) + 8.0).round();
        let stride = MENU_ROW_H + MENU_ROW_GAP;
        let fps_rows = FPS_COLUMNS.iter().map(|c| c.len()).max().unwrap_or(0) as f32;
        let menu_h = MENU_PAD * 2.0
            + header_h
            + stride * (RES_PRESETS.len() + 1) as f32
            + MENU_SECTION_GAP
            + header_h
            + stride * (1.0 + fps_rows);

        let below = anchor.y + anchor.h + MENU_GAP;
        let above = anchor.y - MENU_GAP - menu_h;
        let y = if below + menu_h <= win_h - MENU_PAD {
            below
        } else {
            above.max(MENU_PAD)
        }
        .round();
        // Right-aligned to the gear, since the gear is itself near the right
        // edge; the `max` keeps a narrow window from pushing it off the left.
        let x = (anchor.x + anchor.w - menu_w)
            .min(win_w - menu_w - MENU_PAD)
            .max(MENU_PAD)
            .round();
        self.project_menu_rect = Rect { x, y, w: menu_w, h: menu_h };

        for step in (1..=MENU_SHADOW_STEPS).rev() {
            let g = step as f32;
            self.quads.push(Quad::colored(
                [x - g, y - g + 1.0],
                [menu_w + g * 2.0, menu_h + g * 2.0],
                MENU_SHADOW,
            ));
        }
        self.quads.push(Quad::colored(
            [x - 1.0, y - 1.0],
            [menu_w + 2.0, menu_h + 2.0],
            MENU_BORDER,
        ));
        self.quads
            .push(Quad::colored([x, y], [menu_w, menu_h], MENU_BG));

        let mut cursor_y = y + MENU_PAD;
        let header = |state: &mut Self, label: &str, cy: f32| {
            let ascent = state.text.ascent(MENU_HEADER_SIZE);
            state.text.draw(
                &state.queue,
                &mut state.quads,
                [(x + MENU_PAD).round(), (cy + ascent).round()],
                label,
                MENU_HEADER_SIZE,
                MENU_HEADER_COLOR,
            );
        };

        header(self, "RESOLUTION", cursor_y);
        cursor_y += header_h;
        let res_options = std::iter::once((Setting::Auto, "Match first clip".to_string())).chain(
            RES_PRESETS
                .iter()
                .map(|&(rw, rh)| (Setting::Fixed((rw, rh)), format!("{rw} x {rh}"))),
        );
        for (setting, label) in res_options {
            let rect = Rect { x: x + MENU_PAD, y: cursor_y, w: body_w, h: MENU_ROW_H };
            self.push_menu_row(rect, &label, setting == self.canvas_res, ProjectChoice::Resolution(setting));
            cursor_y += stride;
        }

        cursor_y += MENU_SECTION_GAP;
        header(self, "FRAME RATE", cursor_y);
        cursor_y += header_h;
        let auto_rect = Rect { x: x + MENU_PAD, y: cursor_y, w: body_w, h: MENU_ROW_H };
        self.push_menu_row(
            auto_rect,
            "Match first clip",
            self.canvas_fps == Setting::Auto,
            ProjectChoice::Fps(Setting::Auto),
        );
        cursor_y += stride;
        for (col, rates) in FPS_COLUMNS.iter().enumerate() {
            let col_x = x + MENU_PAD + col as f32 * (MENU_FPS_COL_W + MENU_FPS_COL_GAP);
            for (row, &fps) in rates.iter().enumerate() {
                let rect = Rect {
                    x: col_x,
                    y: cursor_y + row as f32 * stride,
                    w: MENU_FPS_COL_W,
                    h: MENU_ROW_H,
                };
                let setting = Setting::Fixed(fps);
                self.push_menu_row(
                    rect,
                    &fmt_fps(fps),
                    self.canvas_fps == setting,
                    ProjectChoice::Fps(setting),
                );
            }
        }
    }

    fn push_menu_row(&mut self, rect: Rect, label: &str, selected: bool, choice: ProjectChoice) {
        draw_menu_row(
            &mut self.quads,
            &mut self.text,
            &self.queue,
            rect,
            label,
            MENU_ROW_SIZE,
            rect.contains(self.cursor),
            selected,
        );
        self.project_menu_items.push((rect, choice));
    }

    /// Apply a popup choice. The popup stays open: resolution and rate are two
    /// decisions people usually make together, and closing after the first
    /// would mean reopening to make the second.
    fn apply_project_choice(&mut self, choice: ProjectChoice) {
        let before = (self.canvas_res, self.canvas_fps);
        match choice {
            ProjectChoice::Resolution(setting) => self.canvas_res = setting,
            ProjectChoice::Fps(setting) => self.canvas_fps = setting,
        }
        // Canvas settings sit outside the undo system, so they mark the project
        // dirty directly rather than riding along on an edit step. Re-picking
        // the row that is already active changes nothing and shouldn't.
        if before != (self.canvas_res, self.canvas_fps) {
            self.dirty = true;
        }
    }

    /// Name for the title bar and dialogs. Untitled until the project has a
    /// file of its own.
    fn project_display_name(&self) -> &str {
        self.project_path
            .as_deref()
            .and_then(Path::file_name)
            .and_then(|s| s.to_str())
            .unwrap_or("Untitled")
    }

    /// Keep the window title in step with the project and whether it has
    /// unsaved work. Called once a frame; comparing against `title_shown` is
    /// what keeps that from being a window-manager round trip every frame.
    fn update_title(&mut self) {
        let title = format!(
            "{}{} - videoEditor",
            if self.dirty { "• " } else { "" },
            self.project_display_name()
        );
        if title != self.title_shown {
            self.window.set_title(&title);
            self.title_shown = title;
        }
    }

    /// Ask before throwing unsaved work away; `true` means go ahead. A clean
    /// project never prompts, which is what makes the prompt meaningful when
    /// it does appear.
    fn confirm_discard(&self, action: &str) -> bool {
        if !self.dirty {
            return true;
        }
        let name = self.project_display_name();
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Warning)
            .set_title("Unsaved changes")
            .set_description(format!("{action} will discard unsaved changes to {name}."))
            .set_buttons(rfd::MessageButtons::OkCancel)
            .show()
            == rfd::MessageDialogResult::Ok
    }

    /// Gather the session into a serializable project, with every path stored
    /// relative to `dir` where it can be.
    ///
    /// The whole pool goes in, not just the sources the timeline uses: a bin of
    /// imported footage is part of the project even before it reaches a track,
    /// and reopening to find the unused imports gone would be a quiet loss.
    fn as_project(&self, dir: &Path) -> project::Project {
        let mut ids = self.media.ids().to_vec();
        // A clip whose source has left the pool can't currently arise —
        // removing a pool row removes its clips too — but writing a clip that
        // points at a source the file doesn't carry would make a project that
        // silently drops that clip when reopened. Append rather than trust.
        for track in &self.timeline.tracks {
            for clip in &track.clips {
                if !ids.contains(&clip.source) {
                    ids.push(clip.source);
                }
            }
        }
        project::Project {
            version: project::FORMAT_VERSION,
            canvas: project::CanvasSettings {
                resolution: self.canvas_res,
                fps: self.canvas_fps,
            },
            sources: ids
                .iter()
                .filter_map(|&id| {
                    self.media.get(id).map(|src| project::SourceEntry {
                        id,
                        path: project::Project::store_path(Path::new(&src.path), dir),
                    })
                })
                .collect(),
            tracks: self
                .timeline
                .tracks
                .iter()
                .map(|t| project::TrackEntry {
                    kind: t.kind,
                    clips: t.clips.clone(),
                })
                .collect(),
        }
    }

    /// Write the project, asking for a location when it hasn't got one or when
    /// `save_as` forces the dialog.
    fn save_project(&mut self, save_as: bool) {
        let path = match &self.project_path {
            Some(path) if !save_as => path.clone(),
            _ => {
                let Some(picked) = rfd::FileDialog::new()
                    .add_filter("videoEditor project", &[project::EXTENSION])
                    .set_file_name(format!("untitled.{}", project::EXTENSION))
                    .save_file()
                else {
                    return;
                };
                // A picker the user cleared the suffix in would otherwise
                // produce a file the open dialog's own filter then hides.
                if picked.extension().is_some() {
                    picked
                } else {
                    picked.with_extension(project::EXTENSION)
                }
            }
        };

        let dir = path.parent().unwrap_or(Path::new("")).to_path_buf();
        let project = self.as_project(&dir);
        if let Err(e) = project::write(&path, &project) {
            log::error!("failed to save {}: {e}", path.display());
            self.set_status(format!("Save failed: {e}"), STATUS_ERR);
            return;
        }
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("project")
            .to_string();
        self.project_path = Some(path);
        self.dirty = false;
        self.set_status(format!("Saved {name}"), STATUS_OK);
    }

    fn open_project(&mut self) {
        if !self.confirm_discard("Opening another project") {
            return;
        }
        let Some(path) = rfd::FileDialog::new()
            .add_filter("videoEditor project", &[project::EXTENSION])
            .pick_file()
        else {
            return;
        };
        self.load_project(&path);
    }

    /// Replace the session with the project at `path`.
    ///
    /// Media that won't open is reported and its clips are dropped, rather than
    /// refusing the whole file: one moved clip shouldn't make a project
    /// unopenable, and nothing is written back until the next save. There is no
    /// relink UI yet, so saying so loudly is the only warning available.
    fn load_project(&mut self, path: &Path) {
        let loaded = match project::read(path) {
            Ok(loaded) => loaded,
            Err(e) => {
                log::error!("failed to open {}: {e}", path.display());
                self.set_status(format!("Open failed: {e}"), STATUS_ERR);
                return;
            }
        };
        let dir = path.parent().unwrap_or(Path::new("")).to_path_buf();

        // Re-import into a pool of its own. Ids come from the fresh pool rather
        // than the file, so every clip's source has to be remapped through
        // `imported` — a source that failed to open simply has no entry, which
        // is what identifies the clips that can't be kept.
        let mut media = MediaPool::new();
        let mut imported: HashMap<SourceId, SourceId> = HashMap::new();
        let mut missing = 0;
        for entry in &loaded.sources {
            let resolved = project::Project::resolve_path(&entry.path, &dir);
            match resolved
                .to_str()
                .ok_or(ffmpeg_next::Error::InvalidData)
                .and_then(|p| media.add(p, &self.device, &self.queue, &self.quads))
            {
                Ok(new_id) => {
                    imported.insert(entry.id, new_id);
                }
                Err(e) => {
                    log::error!("missing media {}: {e}", resolved.display());
                    missing += 1;
                }
            }
        }

        let mut dropped = 0;
        let mut tracks = Vec::new();
        for entry in &loaded.tracks {
            let mut track = Track::new(entry.kind);
            for clip in &entry.clips {
                match imported.get(&clip.source) {
                    Some(&source) => track.clips.push(Clip { source, ..*clip }),
                    None => dropped += 1,
                }
            }
            tracks.push(track);
        }

        self.audio.set_playing(false);
        self.audio.set_position(0.0);
        self.media = media;
        self.timeline = Timeline::new();
        self.timeline.tracks = tracks;
        self.timeline.reseed_counters();
        self.canvas_res = loaded.canvas.resolution;
        self.canvas_fps = loaded.canvas.fps;
        self.reset_session_state();
        self.project_path = Some(path.to_path_buf());
        self.dirty = false;

        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("project")
            .to_string();
        if missing == 0 {
            self.set_status(format!("Opened {name}"), STATUS_OK);
        } else {
            self.set_status(
                format!("Opened {name}: {missing} media missing, {dropped} clips dropped"),
                STATUS_ERR,
            );
        }
    }

    /// Start over with an empty timeline. Without this, opening a project would
    /// be a one-way door: nothing else gets you back to a blank session short
    /// of relaunching.
    fn new_project(&mut self) {
        if !self.confirm_discard("Starting a new project") {
            return;
        }
        self.audio.set_playing(false);
        self.audio.set_position(0.0);
        self.media = MediaPool::new();
        self.timeline = Timeline::new();
        self.timeline.tracks = default_tracks();
        self.canvas_res = Setting::Auto;
        self.canvas_fps = Setting::Auto;
        self.reset_session_state();
        self.project_path = None;
        self.dirty = false;
    }

    /// Drop everything that pointed into the timeline that was just replaced.
    /// Undo history is the dangerous one: a step from the previous project
    /// restores clips referencing sources this one has never imported.
    fn reset_session_state(&mut self) {
        self.selected = None;
        self.drag = DragMode::None;
        self.last_playing_source = None;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.pending_edit = None;
        self.edit_depth = 0;
    }

    fn can_export(&self) -> bool {
        self.export.is_none() && self.timeline.duration() > 0.0
    }

    fn set_status(&mut self, message: String, color: [f32; 4]) {
        self.status = Some((message, color, Instant::now()));
    }

    fn start_export(&mut self) {
        // Clicking Export while one is running cancels it — the button is the
        // only affordance, so it has to be the stop as well as the start.
        if let Some(job) = &self.export {
            job.cancel();
            self.set_status("Cancelling…".into(), STATUS_INFO);
            return;
        }
        if self.timeline.duration() <= 0.0 {
            self.set_status("Nothing to export".into(), STATUS_ERR);
            return;
        }
        let Some(output) = rfd::FileDialog::new()
            .add_filter("MP4 video", &["mp4"])
            .set_file_name("export.mp4")
            .save_file()
        else {
            return;
        };

        // Snapshot the timeline and resolve every path up front, so the worker
        // renders what you saw when you pressed the button and you stay free to
        // keep editing while it runs.
        let tracks = self
            .timeline
            .tracks
            .iter()
            .map(|t| (t.kind, t.clips.clone()))
            .collect::<Vec<_>>();
        let mut paths = HashMap::new();
        for (_, clips) in &tracks {
            for clip in clips {
                if let Some(src) = self.media.get(clip.source) {
                    paths.insert(clip.source, src.path.clone());
                }
            }
        }

        let request = ExportRequest {
            output,
            video: self.export_video_spec(),
            tracks,
            paths,
        };
        self.export = Some(ExportJob::start(request));
        self.set_status("Starting…".into(), STATUS_INFO);
    }

    /// Retire a finished job and turn its result into a status message. Called
    /// once per frame; the worker reports through a mutex rather than the event
    /// loop, so this poll is what surfaces it.
    fn poll_export(&mut self) {
        let Some(job) = &self.export else {
            return;
        };
        let Some(outcome) = job.take_outcome() else {
            return;
        };
        self.export = None;
        match outcome {
            Outcome::Done(path) => {
                let name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("file")
                    .to_string();
                self.set_status(format!("Exported {name}"), STATUS_OK);
            }
            Outcome::Cancelled => {
                self.set_status("Export cancelled".into(), STATUS_INFO);
            }
            Outcome::Failed(err) => {
                log::error!("export failed: {err}");
                self.set_status(format!("Export failed: {err}"), STATUS_ERR);
            }
        }
    }

    fn render(&mut self) {
        let surface_texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(texture) => texture,
            wgpu::CurrentSurfaceTexture::Occluded | wgpu::CurrentSurfaceTexture::Timeout => return,
            wgpu::CurrentSurfaceTexture::Suboptimal(_) | wgpu::CurrentSurfaceTexture::Outdated => {
                self.configure_surface();
                return;
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                unreachable!("No error scope registered, so validation errors will panic")
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                self.surface = self.instance.create_surface(self.window.clone()).unwrap();
                self.configure_surface();
                return;
            }
        };
        let texture_view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor {
                format: Some(self.surface_format.add_srgb_suffix()),
                ..Default::default()
            });

        // The export worker reports through a mutex, not the event loop, so
        // this poll is what turns a finished render into a status message.
        self.poll_export();

        self.update_cursor_icon();

        let [w, h] = self.logical_size();
        let top_h = self.timeline_top();
        let bottom_h = h - top_h;
        let media_w = self.media_pool_w();
        let preview_w = w - media_w;

        // Clamp playhead to [0, duration]. The audio engine drives time forward
        // while playing; if we ran past the end, pause and park at the end so
        // video and audio agree on "stopped".
        let duration = self.timeline.duration();
        let mut t = self.audio.position();
        if duration <= 0.0 {
            self.audio.set_playing(false);
            self.audio.set_position(0.0);
            t = 0.0;
        } else if t >= duration {
            self.audio.set_playing(false);
            self.audio.set_position(duration);
            t = duration;
        }

        // Refill the audio mix buffer before doing render work. Done early so
        // if render is slow the audio thread still has samples queued.
        {
            let Self {
                audio,
                timeline,
                media,
                ..
            } = self;
            audio.tick(timeline, media);
        }

        let preview_h = (top_h - TRANSPORT_BAR_H).max(0.0);

        self.quads.clear();
        self.quads
            .push(Quad::colored([0.0, 0.0], [media_w, top_h], MEDIA_POOL_COLOR));
        self.quads.push(Quad::colored(
            [media_w, 0.0],
            [preview_w, preview_h],
            PREVIEW_COLOR,
        ));
        self.quads.push(Quad::colored(
            [media_w, preview_h],
            [preview_w, TRANSPORT_BAR_H],
            TRANSPORT_BAR_COLOR,
        ));
        // Panel edges. Drawn just inside the lighter panel so the darker well
        // beside it stays a clean field.
        self.quads.push(Quad::colored(
            [media_w - 1.0, 0.0],
            [1.0, top_h],
            PANEL_BORDER_COLOR,
        ));
        self.quads.push(Quad::colored(
            [media_w, preview_h],
            [preview_w, 1.0],
            PANEL_BORDER_COLOR,
        ));

        // --- Preview: the canvas, and the topmost active clip composed on it ---
        // The canvas is fitted into the panel first, then the clip is fitted
        // into the canvas — two stages, not one. Fitting the clip straight to
        // the panel is what used to hide format mismatches: a 4:3 clip filled a
        // 16:9 preview edge to edge and then exported pillarboxed, with nothing
        // on screen to warn you.
        let canvas = self.canvas();
        let canvas_scale = (preview_w / canvas.width as f32)
            .min(preview_h / canvas.height as f32)
            .max(0.0);
        let canvas_w = (canvas.width as f32 * canvas_scale).round();
        let canvas_h = (canvas.height as f32 * canvas_scale).round();
        let canvas_x = (media_w + (preview_w - canvas_w) * 0.5).round();
        let canvas_y = ((preview_h - canvas_h) * 0.5).round();
        // Black, unlike the near-black panel around it, so the frame is visible
        // as a frame even with nothing playing — that outline is the only thing
        // showing what shape the project is.
        self.quads.push(Quad::colored(
            [canvas_x, canvas_y],
            [canvas_w, canvas_h],
            CANVAS_COLOR,
        ));

        // Scoped disjoint borrows so the decoder advance + textured-quad push can
        // share this block without leaking borrows past it.
        {
            let Self {
                media,
                timeline,
                quads,
                queue,
                last_playing_source,
                ..
            } = self;

            let active_info = timeline
                .topmost_video_clip(t)
                .map(|(_, c)| (c.source, c.source_time(t)));
            if let Some((source_id, source_t)) = active_info {
                *last_playing_source = Some(source_id);
                if let Some(src) = media.get_mut(source_id) {
                    src.stream.goto(queue, source_t);

                    // Placement is computed in canvas pixels and then scaled to
                    // the panel, rather than fitted to the panel directly, so
                    // the preview is a faithful scale model of the export.
                    let (cx, cy, cw, ch) =
                        canvas.fit(src.stream.width() as f32, src.stream.height() as f32);
                    quads.push_with(
                        Quad::textured(
                            [canvas_x + cx * canvas_scale, canvas_y + cy * canvas_scale],
                            [cw * canvas_scale, ch * canvas_scale],
                        ),
                        Some(src.stream.texture()),
                    );
                }
            } else {
                *last_playing_source = None;
            }
        }

        // --- Timeline panel background ---
        self.quads
            .push(Quad::colored([0.0, top_h], [w, bottom_h], TIMELINE_COLOR));
        self.quads
            .push(Quad::colored([0.0, top_h], [w, 1.0], PANEL_BORDER_COLOR));

        // --- Timeline tracks ---
        let tracks_top = top_h + TIMELINE_TOP_PAD;
        let tracks_bottom = h;
        let tracks_area_h = (tracks_bottom - tracks_top).max(0.0);
        // Snap center to a whole pixel so derived lane_y values don't land on
        // half-pixels (which renders as a blurry edge under bilinear sampling).
        let center_y = (tracks_top + tracks_area_h * 0.5).round();
        let half_gap = TRACK_LANE_GAP * 0.5;
        let lane_h = compute_lane_height(tracks_area_h, self.timeline.tracks.len());

        let video_tracks: Vec<usize> = self
            .timeline
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, tr)| tr.kind == TrackKind::Video)
            .map(|(i, _)| i)
            .collect();
        let audio_tracks: Vec<usize> = self
            .timeline
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, tr)| tr.kind == TrackKind::Audio)
            .map(|(i, _)| i)
            .collect();

        // Divider between video (above) and audio (below) regions.
        self.quads.push(Quad::colored(
            [0.0, center_y - 0.5],
            [w, 1.0],
            DIVIDER_COLOR,
        ));

        // Use the real duration so clip widths, playhead, and scrub all share the
        // same denominator. Fold in any currently-dragged clip's duration so the
        // ghost previews at the same scale it'll occupy after drop, instead of
        // ballooning to screen width when the timeline is empty (timeline=0 →
        // `.max(1.0)` → ghost_w = clip_dur * clips_w).
        let ghost_dur = if let DragMode::PoolDrag { source } = self.drag {
            self.media.duration(source)
        } else {
            0.0
        };
        let timeline_duration_display = self.timeline.duration().max(ghost_dur).max(1.0);
        let clips_x = TRACK_HEADER_WIDTH;
        let clips_w = (w - TRACK_HEADER_WIDTH).max(1.0);

        // --- Timeline ruler: flush above the topmost video lane, with tick
        //     marks + timecode labels along the bottom edge so the scale is
        //     legible at a glance.
        let topmost = topmost_lane_top(center_y, lane_h, video_tracks.len());
        let ruler_bottom = topmost;
        let ruler_top = ruler_bottom - TIMELINE_RULER_H;
        self.quads.push(Quad::colored(
            [0.0, ruler_top],
            [w, TIMELINE_RULER_H],
            TIMELINE_RULER_COLOR,
        ));
        // Panel-weight, not divider-weight: this is where the ruler's chrome
        // ends and the clip wells begin.
        self.quads.push(Quad::colored(
            [0.0, ruler_bottom - 1.0],
            [w, 1.0],
            PANEL_BORDER_COLOR,
        ));
        let pps = clips_w / timeline_duration_display as f32;
        let interval = nice_tick_interval(pps);
        let label_size = TIMELINE_RULER_LABEL_SIZE;
        let label_ascent = self.text.ascent(label_size);
        let label_baseline = (ruler_top + label_ascent + 3.0).round();
        let mut i: usize = 0;
        loop {
            let t = i as f64 * interval;
            if t > timeline_duration_display {
                break;
            }
            let x = (clips_x + (t / timeline_duration_display) as f32 * clips_w).round();
            self.quads.push(Quad::colored(
                [x, ruler_bottom - TIMELINE_RULER_TICK_H],
                [1.0, TIMELINE_RULER_TICK_H],
                TIMELINE_RULER_TICK_COLOR,
            ));
            let label = format_tick_label(t, interval);
            let lw = self.text.measure_width(&label, label_size);
            let lx = (x + 3.0).round();
            if lx + lw <= clips_x + clips_w {
                self.text.draw(
                    &self.queue,
                    &mut self.quads,
                    [lx, label_baseline],
                    &label,
                    label_size,
                    TIMELINE_RULER_LABEL_COLOR,
                );
            }
            i += 1;
            if i > 10_000 {
                break;
            }
        }

        // V1 sits just above the divider (leaving half_gap between its bottom
        // and center_y), V2 stacks above V1 with a full TRACK_LANE_GAP between.
        for (visual_i, &track_idx) in video_tracks.iter().enumerate() {
            let lane_y = center_y
                - half_gap
                - lane_h
                - visual_i as f32 * (lane_h + TRACK_LANE_GAP);
            self.draw_track_lane(
                lane_y,
                lane_h,
                clips_x,
                clips_w,
                timeline_duration_display,
                track_idx,
                visual_i,
            );
        }
        // A1 sits just below the divider, A2 below A1, etc.
        for (visual_i, &track_idx) in audio_tracks.iter().enumerate() {
            let lane_y = center_y + half_gap + visual_i as f32 * (lane_h + TRACK_LANE_GAP);
            self.draw_track_lane(
                lane_y,
                lane_h,
                clips_x,
                clips_w,
                timeline_duration_display,
                track_idx,
                visual_i,
            );
        }

        // --- Playhead: drawn last so it's on top of clips ---
        // Starts at the ruler rather than the top of the panel: it marks a
        // position on the time scale, and the toolbar above has no time axis
        // for it to point at.
        if self.timeline.duration() > 0.0 {
            let ratio = (t / self.timeline.duration()).clamp(0.0, 1.0) as f32;
            let px = (clips_x + ratio * clips_w - PLAYHEAD_WIDTH * 0.5).round();
            self.quads.push(Quad::colored(
                [px, ruler_top],
                [PLAYHEAD_WIDTH, h - ruler_top],
                PLAYHEAD_COLOR,
            ));
        }

        // --- Pool-drag ghost: previews where the clip will land ---
        // Start-aligned to the cursor (matches `end_drag`'s drop_t semantics)
        // and snapped to the hovered lane's y when over one, so the preview
        // rect is exactly the rect that'll be created on mouse-up.
        if let DragMode::PoolDrag { source } = self.drag {
            let dur = self.media.duration(source);
            let ghost_w = ((dur / timeline_duration_display) as f32 * clips_w).max(40.0);
            let ghost_h = lane_h;
            let layout = self.timeline_layout();
            let over_lane = self.track_at_y(self.cursor[1], &layout);
            let gx = self.cursor[0].max(clips_x);
            let (gy, ghost_color) = match over_lane {
                Some(track_idx) => match self.timeline.tracks[track_idx].kind {
                    TrackKind::Video => {
                        let visual_i =
                            video_tracks.iter().position(|&i| i == track_idx).unwrap_or(0);
                        let y = center_y
                            - half_gap
                            - lane_h
                            - visual_i as f32 * (lane_h + TRACK_LANE_GAP);
                        (y, DRAG_GHOST_VIDEO_COLOR)
                    }
                    TrackKind::Audio => {
                        let visual_i =
                            audio_tracks.iter().position(|&i| i == track_idx).unwrap_or(0);
                        let y = center_y + half_gap + visual_i as f32 * (lane_h + TRACK_LANE_GAP);
                        (y, DRAG_GHOST_AUDIO_COLOR)
                    }
                },
                None => (self.cursor[1] - ghost_h * 0.5, DRAG_GHOST_VIDEO_COLOR),
            };
            self.quads
                .push(Quad::colored([gx, gy], [ghost_w, ghost_h], ghost_color));
        }

        // --- Media pool list ---
        self.draw_media_pool_list(media_w, top_h);

        // --- Panel labels ---
        let baseline_y = LABEL_PAD + self.text.ascent(LABEL_SIZE);
        self.text.draw(
            &self.queue,
            &mut self.quads,
            [LABEL_PAD, baseline_y],
            "MEDIA POOL",
            LABEL_SIZE,
            LABEL_COLOR,
        );

        // --- Media pool toolbar: just right of the MEDIA POOL label ---
        let pool_label_w = self.text.measure_width("MEDIA POOL", LABEL_SIZE);
        self.pool_open_btn = Rect {
            x: (LABEL_PAD + pool_label_w + LABEL_PAD * 1.5).round(),
            y: ((POOL_LIST_TOP - TRANSPORT_BTN_H) * 0.5).round(),
            w: TRANSPORT_BTN_W,
            h: TRANSPORT_BTN_H,
        };
        let pool_open_hovered = self.pool_open_btn.contains(self.cursor);
        draw_button(
            &mut self.quads,
            &mut self.text,
            &self.queue,
            self.pool_open_btn,
            // `import`, not `folder-open`: that glyph now means "open a
            // project" in the timeline toolbar, and one icon standing for two
            // different actions in the same window is worse than either choice.
            ICON_IMPORT,
            TRANSPORT_ICON_SIZE,
            pool_open_hovered,
            BtnState::Normal,
        );
        if pool_open_hovered {
            draw_tooltip(
                &mut self.quads,
                &mut self.text,
                &self.queue,
                self.pool_open_btn,
                "Import media (O)",
                TRANSPORT_TOOLTIP_SIZE,
                TooltipSide::Below,
                w,
            );
        }
        self.text.draw(
            &self.queue,
            &mut self.quads,
            [media_w + LABEL_PAD, baseline_y],
            "PREVIEW",
            LABEL_SIZE,
            LABEL_COLOR,
        );
        // --- Timeline toolbar: the label and its buttons share one row ---
        // Both are centered on the same line, so widening the band moves them
        // down together instead of drifting apart.
        let toolbar_center_y = top_h + TIMELINE_TOP_PAD * 0.5;
        self.text.draw(
            &self.queue,
            &mut self.quads,
            [
                LABEL_PAD,
                (toolbar_center_y + self.text.ascent(LABEL_SIZE) * 0.5).round(),
            ],
            "TIMELINE",
            LABEL_SIZE,
            LABEL_COLOR,
        );

        let timeline_label_w = self.text.measure_width("TIMELINE", LABEL_SIZE);
        let btn_y = (toolbar_center_y - TRANSPORT_BTN_H * 0.5).round();
        let btn_x = (LABEL_PAD + timeline_label_w + LABEL_PAD * 1.5).round();
        let stride = TRANSPORT_BTN_W + TRANSPORT_GAP;
        self.timeline_split_btn = Rect {
            x: btn_x,
            y: btn_y,
            w: TRANSPORT_BTN_W,
            h: TRANSPORT_BTN_H,
        };
        // Delete sits next to Split — both act on clips — with undo/redo after
        // them. Those live here rather than in a window-level toolbar because
        // history covers timeline + pool edits, which is what this strip
        // governs.
        self.timeline_delete_btn = Rect {
            x: btn_x + stride,
            ..self.timeline_split_btn
        };
        self.timeline_undo_btn = Rect {
            x: btn_x + stride * 2.0,
            ..self.timeline_split_btn
        };
        self.timeline_redo_btn = Rect {
            x: btn_x + stride * 3.0,
            ..self.timeline_split_btn
        };
        self.timeline_snap_btn = Rect {
            x: btn_x + stride * 4.0,
            ..self.timeline_split_btn
        };
        // Pinned to the right edge rather than trailing the cluster: Export
        // ends the workflow the other buttons edit, and the distance says so.
        self.timeline_export_btn = Rect {
            x: (w - LABEL_PAD - EXPORT_BTN_W).round(),
            y: btn_y,
            w: EXPORT_BTN_W,
            h: TRANSPORT_BTN_H,
        };
        // Immediately left of Export, because it decides what Export produces.
        self.timeline_project_btn = Rect {
            x: (self.timeline_export_btn.x - TRANSPORT_GAP - TRANSPORT_BTN_W).round(),
            y: btn_y,
            w: TRANSPORT_BTN_W,
            h: TRANSPORT_BTN_H,
        };
        // Save then Open, continuing leftward. Ordered so the pair reads
        // open-then-save left to right, the order you do them in.
        self.timeline_save_btn = Rect {
            x: (self.timeline_project_btn.x - TRANSPORT_GAP - TRANSPORT_BTN_W).round(),
            ..self.timeline_project_btn
        };
        self.timeline_open_btn = Rect {
            x: (self.timeline_save_btn.x - TRANSPORT_GAP - TRANSPORT_BTN_W).round(),
            ..self.timeline_project_btn
        };
        let exporting = self.export.is_some();
        // The gear's tooltip is the only place the resolved canvas is spelled
        // out, which matters most when both settings are on Auto and the popup
        // just says "Match first clip".
        let project_tip = format!(
            "Project canvas: {}x{} @ {}",
            canvas.width,
            canvas.height,
            fmt_fps(canvas.fps)
        );

        let avail = |yes: bool| {
            if yes {
                BtnState::Normal
            } else {
                BtnState::Disabled
            }
        };
        let buttons = [
            (
                self.timeline_open_btn,
                ICON_OPEN,
                "Open project (Ctrl+O)",
                BtnState::Normal,
            ),
            (
                self.timeline_save_btn,
                ICON_SAVE,
                // Greying out when there is nothing to save is what gives the
                // dirty state a home inside the window, rather than only in
                // the title bar where a maximised window hides it.
                if self.dirty {
                    "Save project (Ctrl+S)"
                } else {
                    "No unsaved changes"
                },
                avail(self.dirty),
            ),
            (
                self.timeline_split_btn,
                ICON_SPLIT,
                "Split at playhead (S)",
                BtnState::Normal,
            ),
            (
                self.timeline_delete_btn,
                ICON_DELETE,
                "Delete clip (Del)",
                avail(self.has_selection()),
            ),
            (
                self.timeline_undo_btn,
                ICON_UNDO,
                "Undo (Ctrl+Z)",
                avail(!self.undo_stack.is_empty()),
            ),
            (
                self.timeline_redo_btn,
                ICON_REDO,
                "Redo (Ctrl+Shift+Z)",
                avail(!self.redo_stack.is_empty()),
            ),
            (
                self.timeline_snap_btn,
                ICON_SNAP,
                "Snap to clip edges (N)",
                BtnState::Toggle(self.snap_enabled),
            ),
            (
                self.timeline_project_btn,
                ICON_SETTINGS,
                project_tip.as_str(),
                BtnState::Toggle(self.project_menu_open),
            ),
            (
                self.timeline_export_btn,
                if exporting { ICON_STOP } else { ICON_RENDER },
                if exporting {
                    "Cancel this export"
                } else {
                    "Export to MP4 (Ctrl+E)"
                },
                avail(exporting || self.can_export()),
            ),
        ];
        for (rect, icon, tip, state) in buttons {
            let hovered = rect.contains(self.cursor);
            draw_button(
                &mut self.quads,
                &mut self.text,
                &self.queue,
                rect,
                icon,
                TRANSPORT_ICON_SIZE,
                hovered,
                state,
            );
            // Tooltip even when disabled — it's how you learn the shortcut.
            if hovered {
                draw_tooltip(
                    &mut self.quads,
                    &mut self.text,
                    &self.queue,
                    rect,
                    tip,
                    TRANSPORT_TOOLTIP_SIZE,
                    TooltipSide::Below,
                    w,
                );
            }
        }

        // --- Export readout: progress while rendering, result once done ---
        // Both share the strip left of the Export button, so a finished message
        // appears exactly where the bar that produced it was.
        let readout_right = self.timeline_open_btn.x - EXPORT_READOUT_GAP;
        let readout_left = readout_right - EXPORT_READOUT_W;
        let status_ascent = self.text.ascent(STATUS_SIZE);
        if let Some(job) = &self.export {
            let progress = job.progress();
            let block_h = status_ascent + 3.0 + EXPORT_BAR_H;
            let block_top = (toolbar_center_y - block_h * 0.5).round();
            let text = format!("Exporting… {}%", (progress.fraction() * 100.0).round());
            let text_w = self.text.measure_width(&text, STATUS_SIZE);
            self.text.draw(
                &self.queue,
                &mut self.quads,
                [
                    (readout_right - text_w).round(),
                    block_top + status_ascent,
                ],
                &text,
                STATUS_SIZE,
                STATUS_INFO,
            );
            let bar_y = (block_top + status_ascent + 3.0).round();
            self.quads.push(Quad::colored(
                [readout_left, bar_y],
                [EXPORT_READOUT_W, EXPORT_BAR_H],
                EXPORT_BAR_TRACK,
            ));
            let filled = (EXPORT_READOUT_W * progress.fraction()).round();
            if filled > 0.0 {
                self.quads.push(Quad::colored(
                    [readout_left, bar_y],
                    [filled, EXPORT_BAR_H],
                    EXPORT_BAR_FILL,
                ));
            }
        } else if let Some((message, color, since)) = &self.status {
            if since.elapsed().as_secs_f64() < STATUS_SECONDS {
                let (message, color) = (message.clone(), *color);
                let message = truncate_to_width(
                    &self.text,
                    &message,
                    STATUS_SIZE,
                    EXPORT_READOUT_W,
                );
                let text_w = self.text.measure_width(&message, STATUS_SIZE);
                self.text.draw(
                    &self.queue,
                    &mut self.quads,
                    [
                        (readout_right - text_w).round(),
                        (toolbar_center_y + status_ascent * 0.5).round(),
                    ],
                    &message,
                    STATUS_SIZE,
                    color,
                );
            } else {
                self.status = None;
            }
        }

        // --- Transport bar: prev / play / next centered; timer right-aligned ---
        let bar_y = preview_h;
        let bar_center_y = bar_y + TRANSPORT_BAR_H * 0.5;
        let playing = self.audio.playing();
        let icons = [
            ICON_PREV_EDIT,
            ICON_PREV_FRAME,
            if playing { ICON_PAUSE } else { ICON_PLAY },
            ICON_NEXT_FRAME,
            ICON_NEXT_EDIT,
        ];
        let tooltips = [
            "Prev edit (Shift+Left)",
            "Prev frame (Left)",
            if playing { "Pause (Space)" } else { "Play (Space)" },
            "Next frame (Right)",
            "Next edit (Shift+Right)",
        ];
        let n_transport = self.transport.len();
        let row_w = TRANSPORT_BTN_W * n_transport as f32
            + TRANSPORT_GAP * (n_transport - 1) as f32;
        let row_x = (media_w + (preview_w - row_w) * 0.5).round();
        let row_y = (bar_center_y - TRANSPORT_BTN_H * 0.5).round();
        for i in 0..n_transport {
            self.transport[i] = Rect {
                x: row_x + i as f32 * (TRANSPORT_BTN_W + TRANSPORT_GAP),
                y: row_y,
                w: TRANSPORT_BTN_W,
                h: TRANSPORT_BTN_H,
            };
        }

        let timer_text = format!(
            "{} / {}",
            format_timecode(t),
            format_timecode(self.timeline.duration())
        );
        let timer_w = self.text.measure_width(&timer_text, TIMER_SIZE);
        let timer_ascent = self.text.ascent(TIMER_SIZE);
        let timer_baseline = (bar_center_y + timer_ascent * 0.5).round();
        let timer_left = (w - LABEL_PAD - timer_w).round();
        self.text.draw(
            &self.queue,
            &mut self.quads,
            [timer_left, timer_baseline],
            &timer_text,
            TIMER_SIZE,
            TIMER_COLOR,
        );
        let hovered = (0..n_transport).find(|&i| self.transport[i].contains(self.cursor));
        for i in 0..n_transport {
            draw_button(
                &mut self.quads,
                &mut self.text,
                &self.queue,
                self.transport[i],
                icons[i],
                TRANSPORT_ICON_SIZE,
                hovered == Some(i),
                BtnState::Normal,
            );
        }
        if let Some(i) = hovered {
            draw_tooltip(
                &mut self.quads,
                &mut self.text,
                &self.queue,
                self.transport[i],
                tooltips[i],
                TRANSPORT_TOOLTIP_SIZE,
                TooltipSide::Above,
                w,
            );
        }

        // --- Splitter handle: last, so it sits over both panels it divides ---
        // A grabbed divider stays lit even once the cursor has run past a panel
        // minimum and left the line behind, which is the only feedback saying
        // the drag is still live and simply has nowhere further to go.
        let active = match self.drag {
            DragMode::Splitter(s) => Some(s),
            DragMode::None => self.splitter_at(self.cursor),
            _ => None,
        };
        match active {
            // Centered on the 1pt edge each panel already draws, so lighting up
            // moves nothing.
            Some(Splitter::TopBottom) => self.quads.push(Quad::colored(
                [0.0, top_h - (SPLITTER_ACTIVE_W - 1.0) * 0.5],
                [w, SPLITTER_ACTIVE_W],
                SPLITTER_ACTIVE_COLOR,
            )),
            Some(Splitter::PoolPreview) => self.quads.push(Quad::colored(
                [media_w - 1.0 - (SPLITTER_ACTIVE_W - 1.0) * 0.5, 0.0],
                [SPLITTER_ACTIVE_W, top_h],
                SPLITTER_ACTIVE_COLOR,
            )),
            None => {}
        }

        // --- Project settings popup: topmost, and first to be hit-tested ---
        if self.project_menu_open {
            self.draw_project_menu(self.timeline_project_btn, w, h);
        }

        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &texture_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            self.quads
                .draw(&self.device, &self.queue, &mut pass, [w, h]);
        }

        self.queue.submit([encoder.finish()]);
        self.window.pre_present_notify();
        surface_texture.present();
    }

    fn draw_media_pool_list(&mut self, pool_w: f32, pool_h: f32) {
        let row_x = LABEL_PAD;
        let row_w = (pool_w - LABEL_PAD * 2.0).max(1.0);

        for (i, &id) in self.media.ids().iter().enumerate() {
            let row_y = POOL_LIST_TOP + i as f32 * (POOL_ROW_HEIGHT + POOL_ROW_GAP);
            if row_y + POOL_ROW_HEIGHT > pool_h {
                break; // beyond panel; scrolling will come later
            }
            let Some(src) = self.media.get(id) else {
                continue;
            };

            self.quads.push(Quad::colored(
                [row_x, row_y],
                [row_w, POOL_ROW_HEIGHT],
                POOL_ROW_COLOR,
            ));

            // Thumbnail slot (dark background so letterboxed thumbs look intentional).
            let slot_x = row_x + POOL_ROW_PAD;
            let slot_y = row_y + POOL_ROW_PAD;
            self.quads.push(Quad::colored(
                [slot_x, slot_y],
                [POOL_THUMB_W, POOL_THUMB_H],
                POOL_THUMB_BG,
            ));

            // Fit the baked thumbnail into the slot, preserving source aspect.
            let thumb = src.stream.thumbnail();
            let tw = thumb.width as f32;
            let th = thumb.height as f32;
            let scale = (POOL_THUMB_W / tw).min(POOL_THUMB_H / th);
            let dw = (tw * scale).round();
            let dh = (th * scale).round();
            let dx = (slot_x + (POOL_THUMB_W - dw) * 0.5).round();
            let dy = (slot_y + (POOL_THUMB_H - dh) * 0.5).round();
            self.quads
                .push_with(Quad::textured([dx, dy], [dw, dh]), Some(thumb));

            // Duration pill in the bottom-right of the thumb slot.
            let dur_text = format_timecode(src.stream.duration());
            let dur_w = self.text.measure_width(&dur_text, POOL_ITEM_META_SIZE);
            let dur_ascent = self.text.ascent(POOL_ITEM_META_SIZE);
            let pill_pad_x = 4.0;
            let pill_pad_y = 2.0;
            let pill_w = dur_w + pill_pad_x * 2.0;
            let pill_h = dur_ascent + pill_pad_y * 2.0;
            let pill_inset = 3.0;
            let pill_x = slot_x + POOL_THUMB_W - pill_inset - pill_w;
            let pill_y = slot_y + POOL_THUMB_H - pill_inset - pill_h;
            self.quads
                .push(Quad::colored([pill_x, pill_y], [pill_w, pill_h], POOL_DUR_BG));
            self.text.draw(
                &self.queue,
                &mut self.quads,
                [pill_x + pill_pad_x, pill_y + pill_pad_y + dur_ascent],
                &dur_text,
                POOL_ITEM_META_SIZE,
                POOL_DUR_TEXT,
            );

            // Name and format line to the right of the thumb, the pair centered
            // as a block. Both clamp with an ellipsis rather than bleeding into
            // the preview — a narrow pool loses the tail of the filename, which
            // is the half worth losing.
            //
            // The format is worth the second line because it's what decides
            // whether a clip matches the canvas, and `Setting::Auto` means the
            // canvas is inherited from one of these rows: without it, the only
            // way to see what you're inheriting is to look at the export.
            let name_x = slot_x + POOL_THUMB_W + POOL_ROW_PAD + 4.0;
            let name_max_w = (row_x + row_w - POOL_ROW_PAD - name_x).max(0.0);
            let name_ascent = self.text.ascent(POOL_ITEM_NAME_SIZE);
            let meta_ascent = self.text.ascent(POOL_ITEM_META_SIZE);
            let block_h = name_ascent + POOL_ITEM_META_GAP + meta_ascent;
            let name_baseline = (row_y + (POOL_ROW_HEIGHT - block_h) * 0.5 + name_ascent).round();
            let meta_baseline = name_baseline + POOL_ITEM_META_GAP + meta_ascent;
            let name = truncate_to_width(&self.text, &src.name, POOL_ITEM_NAME_SIZE, name_max_w);
            self.text.draw(
                &self.queue,
                &mut self.quads,
                [name_x, name_baseline],
                &name,
                POOL_ITEM_NAME_SIZE,
                CLIP_LABEL_COLOR,
            );

            // ASCII throughout: the UI font is a stock TTF rather than a subset
            // we control, so a '×' or '·' that turned out to be missing would
            // fail as a blank rather than as a build error.
            let meta = format!(
                "{}x{} @ {} fps",
                src.stream.width(),
                src.stream.height(),
                fmt_fps(src.stream.frame_rate()),
            );
            let meta = truncate_to_width(&self.text, &meta, POOL_ITEM_META_SIZE, name_max_w);
            self.text.draw(
                &self.queue,
                &mut self.quads,
                [name_x, meta_baseline],
                &meta,
                POOL_ITEM_META_SIZE,
                POOL_ITEM_META_COLOR,
            );

            let row_hovered = self.cursor[0] >= row_x
                && self.cursor[0] <= row_x + row_w
                && self.cursor[1] >= row_y
                && self.cursor[1] <= row_y + POOL_ROW_HEIGHT;
            if row_hovered {
                let close = pool_row_close_rect(row_x, row_y, row_w);
                let close_hover = close.contains(self.cursor);
                let bg = if close_hover {
                    POOL_CLOSE_BG_HOVER
                } else {
                    POOL_CLOSE_BG
                };
                self.quads
                    .push(Quad::colored([close.x, close.y], [close.w, close.h], bg));
                let glyph = ICON_CLOSE.to_string();
                let gw = self.text.measure_width(&glyph, POOL_CLOSE_LABEL_SIZE);
                let (gh, gymin) =
                    self.text.glyph_visual_bounds(ICON_CLOSE, POOL_CLOSE_LABEL_SIZE);
                let gx = (close.x + (close.w - gw) * 0.5).round();
                let gy = (close.y + (close.h + gh) * 0.5 + gymin).round();
                self.text.draw(
                    &self.queue,
                    &mut self.quads,
                    [gx, gy],
                    &glyph,
                    POOL_CLOSE_LABEL_SIZE,
                    [0.95, 0.95, 0.98, 1.0],
                );
            }
        }
    }

    fn draw_track_lane(
        &mut self,
        lane_y: f32,
        lane_h: f32,
        clips_x: f32,
        clips_w: f32,
        timeline_duration: f64,
        track_idx: usize,
        visual_i: usize,
    ) {
        let track = &self.timeline.tracks[track_idx];
        let (clip_color, label_prefix) = match track.kind {
            TrackKind::Video => (VIDEO_CLIP_COLOR, "V"),
            TrackKind::Audio => (AUDIO_CLIP_COLOR, "A"),
        };
        let unselected_border = darken(clip_color, CLIP_BORDER_DARKEN);

        // Lane background.
        self.quads.push(Quad::colored(
            [0.0, lane_y],
            [clips_x + clips_w, lane_h],
            LANE_COLOR,
        ));

        // Track header label (V1, V2, A1, ...).
        let header = format!("{}{}", label_prefix, visual_i + 1);
        let baseline = lane_y + (lane_h + self.text.ascent(CLIP_LABEL_SIZE)) * 0.5;
        self.text.draw(
            &self.queue,
            &mut self.quads,
            [8.0, baseline],
            &header,
            CLIP_LABEL_SIZE,
            TRACK_LABEL_COLOR,
        );

        // Clips.
        for clip in &track.clips {
            let x = clips_x + (clip.timeline_start / timeline_duration) as f32 * clips_w;
            let cw = ((clip.duration() / timeline_duration) as f32 * clips_w).max(1.0);
            // Selection reads as a brighter version of the clip plus an accent
            // outline, rather than a colour of its own: the blue/green split
            // between video and audio is load-bearing, so recolouring the fill
            // outright would cost more information than it gives.
            let selected = self.selected == Some(clip.id);
            let (fill, border_color, b) = if selected {
                (
                    lighten(clip_color, CLIP_SELECTED_LIFT),
                    CLIP_SELECTED_BORDER,
                    CLIP_SELECTED_BORDER_PX,
                )
            } else {
                (clip_color, unselected_border, CLIP_BORDER_PX)
            };
            self.quads
                .push(Quad::colored([x, lane_y], [cw, lane_h], fill));

            // Waveform bars for audio clips. One 1px-wide vertical rect per
            // pixel column, height proportional to the max peak in that
            // column's source-time window. Label is drawn after so it sits
            // on top of the waveform.
            if track.kind == TrackKind::Audio && cw > 1.0 {
                if let Some(src) = self.media.get(clip.source) {
                    if let Some(wf) = src.waveform.as_ref() {
                        if !wf.peaks.is_empty() {
                            let clip_dur = clip.duration();
                            let seconds_per_px = clip_dur / cw as f64;
                            let mid_y = lane_y + lane_h * 0.5;
                            let max_half_h = (lane_h * 0.45_f32).max(1.0);
                            let n_cols = cw.ceil() as i32;
                            let n_peaks = wf.peaks.len();
                            for col in 0..n_cols {
                                let src_t_start =
                                    clip.source_in + col as f64 * seconds_per_px;
                                let src_t_end = src_t_start + seconds_per_px;
                                let idx_start =
                                    (src_t_start / wf.bucket_seconds) as usize;
                                let mut idx_end = ((src_t_end / wf.bucket_seconds)
                                    .ceil()
                                    as usize)
                                    .max(idx_start + 1);
                                if idx_start >= n_peaks {
                                    break;
                                }
                                idx_end = idx_end.min(n_peaks);
                                if idx_start >= idx_end {
                                    continue;
                                }
                                let mut peak = 0.0f32;
                                for &p in &wf.peaks[idx_start..idx_end] {
                                    if p > peak {
                                        peak = p;
                                    }
                                }
                                let half_h = (peak * max_half_h).max(0.5);
                                let px = x + col as f32;
                                self.quads.push(Quad::colored(
                                    [px, mid_y - half_h],
                                    [1.0, half_h * 2.0],
                                    AUDIO_WAVE_COLOR,
                                ));
                            }
                        }
                    }
                }
            }

            // Outline, drawn after the waveform so peaks can't paint over it.
            // Every clip gets one, so two butt-joined clips — the halves of a
            // split — meet in a 2px seam. Deliberately not a gap: a split
            // leaves no gap, and faking one would look identical to genuine
            // empty space between clips. For the same reason the outline is a
            // darkened tint of the clip rather than the lane color, so a seam
            // stays distinguishable from a real gap.
            self.quads
                .push(Quad::colored([x, lane_y], [cw, b], border_color));
            self.quads.push(Quad::colored(
                [x, lane_y + lane_h - b],
                [cw, b],
                border_color,
            ));
            // A sliver narrower than its own two edges would render as solid
            // border; leave it as plain clip color instead.
            if cw >= b * 3.0 {
                self.quads
                    .push(Quad::colored([x, lane_y], [b, lane_h], border_color));
                self.quads.push(Quad::colored(
                    [x + cw - b, lane_y],
                    [b, lane_h],
                    border_color,
                ));
            }

            if let Some(src) = self.media.get(clip.source) {
                let label_pad = 6.0;
                let label_max_w = (cw - label_pad * 2.0).max(0.0);
                let label_baseline = lane_y + self.text.ascent(CLIP_LABEL_SIZE) + 4.0;
                let name = truncate_to_width(
                    &self.text,
                    &src.name,
                    CLIP_LABEL_SIZE,
                    label_max_w,
                );
                self.text.draw(
                    &self.queue,
                    &mut self.quads,
                    [x + label_pad, label_baseline],
                    &name,
                    CLIP_LABEL_SIZE,
                    CLIP_LABEL_COLOR,
                );
            }
        }
    }
}

#[derive(Default)]
struct App {
    state: Option<State>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let window = Arc::new(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("Ruve")
                        .with_inner_size(LogicalSize::new(1920.0, 1080.0)),
                )
                .unwrap(),
        );

        let state = pollster::block_on(State::new(
            event_loop.owned_display_handle(),
            window.clone(),
        ));
        self.state = Some(state);

        window.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let state = self.state.as_mut().unwrap();
        match event {
            WindowEvent::CloseRequested => {
                // The one place unsaved work can vanish without the user
                // choosing to lose it, so it's the one place worth a prompt.
                if state.confirm_discard("Quitting") {
                    event_loop.exit();
                }
            }
            WindowEvent::DroppedFile(path) => {
                if let Some(path_str) = path.to_str() {
                    state.import_file(path_str);
                }
            }
            WindowEvent::RedrawRequested => {
                state.update_title();
                state.render();
                state.get_window().request_redraw();
            }
            WindowEvent::Resized(size) => {
                state.resize(size);
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                state.set_scale(scale_factor as f32);
            }
            WindowEvent::CursorMoved { position, .. } => {
                // To points, so hit testing shares a coordinate space with the
                // rects that were drawn.
                let position = position.to_logical::<f32>(state.scale as f64);
                state.cursor = [position.x, position.y];
                state.update_drag();
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                state.begin_drag();
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } => {
                state.end_drag();
            }
            WindowEvent::ModifiersChanged(mods) => {
                state.modifiers = mods.state();
            }
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        physical_key: PhysicalKey::Code(code),
                        state: ElementState::Pressed,
                        repeat,
                        ..
                    },
                ..
            } => {
                let ctrl = state.modifiers.control_key();
                let shift = state.modifiers.shift_key();
                match code {
                    // Arrows repeat so holding steps through frames, or walks
                    // through edit points with Shift.
                    KeyCode::ArrowLeft if shift => state.goto_edit_point(false),
                    KeyCode::ArrowRight if shift => state.goto_edit_point(true),
                    KeyCode::ArrowLeft => state.step_frame(-1.0),
                    KeyCode::ArrowRight => state.step_frame(1.0),
                    // Undo/redo repeat too — holding Ctrl+Z to walk back
                    // through history is the expected feel.
                    KeyCode::KeyZ if ctrl && shift => state.redo(),
                    KeyCode::KeyZ if ctrl => state.undo(),
                    KeyCode::KeyY if ctrl => state.redo(),
                    // The rest are edge-triggered to avoid repeat spam.
                    _ if repeat => {}
                    KeyCode::Escape => state.project_menu_open = false,
                    KeyCode::KeyE if ctrl => state.start_export(),
                    KeyCode::KeyS if ctrl => state.save_project(shift),
                    KeyCode::KeyO if ctrl => state.open_project(),
                    KeyCode::KeyN if ctrl => state.new_project(),
                    // Backspace too: both keys mean "delete" depending on the
                    // keyboard you grew up with.
                    KeyCode::Delete | KeyCode::Backspace => state.delete_selected(),
                    // Guarded on ctrl so the file-management combos above win
                    // rather than falling through to the bare-key action.
                    KeyCode::Space if !ctrl => state.toggle_playback(),
                    KeyCode::KeyO if !ctrl => state.open_file_picker(),
                    KeyCode::KeyS if !ctrl => state.split_at_playhead(),
                    KeyCode::KeyN if !ctrl => {
                        state.snap_enabled = !state.snap_enabled;
                    }
                    _ => {}
                }
            }
            _ => (),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Snap offsets are differences of decimal literals, so they carry the
    /// usual float dust; compare within a tolerance far tighter than any
    /// audible or visible difference.
    fn assert_close(got: Option<f64>, want: f64) {
        let got = got.expect("expected a snap");
        assert!((got - want).abs() < 1e-9, "got {got}, want {want}");
    }

    #[test]
    fn snaps_the_trailing_edge_flush_against_a_neighbour() {
        // Clip [0,10) dragged so its end sits at 9.7; a neighbour starts at
        // 10. The gap closes exactly, leaving no overlap and no seam.
        assert_close(nearest_snap(&[-0.3, 9.7], &[10.0, 15.0], 0.5), 0.3);
    }

    #[test]
    fn snaps_by_whichever_edge_is_closest() {
        // Leading edge is 0.1 from a target, trailing edge 0.4 from another.
        // The leading edge wins even though both are in range.
        assert_close(nearest_snap(&[4.9, 14.6], &[5.0, 15.0], 0.5), 0.1);
    }

    #[test]
    fn nothing_latches_outside_the_threshold() {
        assert_eq!(nearest_snap(&[4.0], &[5.0], 0.5), None);
    }

    #[test]
    fn snapping_pulls_backwards_too() {
        // Overlapping a neighbour by 0.2 pulls back out to flush, so a drag
        // that overshoots still lands clean.
        assert_close(nearest_snap(&[10.2], &[10.0], 0.5), -0.2);
    }

    #[test]
    fn no_targets_means_no_snap() {
        assert_eq!(nearest_snap(&[4.0, 9.0], &[], 0.5), None);
    }
}

fn main() {
    env_logger::init();

    let event_loop = EventLoop::new().unwrap();
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut app = App::default();
    event_loop.run_app(&mut app).unwrap();
}
