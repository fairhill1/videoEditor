//! Every colour, type step and layout metric the UI draws with.
//!
//! Split out of `main.rs` so a value has one home: a new piece of chrome picks
//! a constant from here rather than inventing one at its call site, and a
//! change to the surface scale or the type scale lands in one file.
//!
//! Sizes are in logical points, not pixels. The projection divides by
//! [`crate::state::State::scale`], so a constant holds its physical size across
//! displays and a future UI zoom folds in at that one place rather than at
//! every call site.

// Lucide glyphs, from the subset in `assets/fonts/lucide-subset.ttf`. Named
// here rather than spelled inline so a codepoint appears exactly once — they
// are unreadable on sight, and `tools/build-icon-font.sh` is what keeps this
// list and the font in step.
pub(crate) const ICON_PREV_EDIT: char = '\u{E243}'; // chevron-first
pub(crate) const ICON_PREV_FRAME: char = '\u{E06E}'; // chevron-left
pub(crate) const ICON_PLAY: char = '\u{E13C}'; // play
pub(crate) const ICON_PAUSE: char = '\u{E12E}'; // pause
pub(crate) const ICON_NEXT_FRAME: char = '\u{E06F}'; // chevron-right
pub(crate) const ICON_NEXT_EDIT: char = '\u{E244}'; // chevron-last
pub(crate) const ICON_SPLIT: char = '\u{E3B6}'; // square-split-horizontal
pub(crate) const ICON_DELETE: char = '\u{E18D}'; // trash
pub(crate) const ICON_UNDO: char = '\u{E19B}'; // undo
pub(crate) const ICON_REDO: char = '\u{E143}'; // redo
pub(crate) const ICON_SNAP: char = '\u{E2B5}'; // magnet
pub(crate) const ICON_RENDER: char = '\u{E0D0}'; // film
pub(crate) const ICON_STOP: char = '\u{E167}'; // square
pub(crate) const ICON_IMPORT: char = '\u{E22F}'; // import
pub(crate) const ICON_CLOSE: char = '\u{E1B2}'; // x
pub(crate) const ICON_SETTINGS: char = '\u{E154}'; // settings (gear)
pub(crate) const ICON_OPEN: char = '\u{E247}'; // folder-open
pub(crate) const ICON_SAVE: char = '\u{E14D}'; // save (floppy)

// Starting layout split ratios. Both are draggable at runtime and live on
// `State` from then on; these are only where a fresh session begins.
pub(crate) const TOP_BOTTOM_SPLIT: f32 = 0.55;
pub(crate) const MEDIA_PREVIEW_SPLIT: f32 = 0.28;

// Splitter behavior.
/// How far either side of a divider counts as grabbing it. Generous next to
/// `CLIP_EDGE_GRAB_PX`, because missing a splitter is worse than missing a trim
/// handle: the click lands on whatever is behind it and scrubs or deselects.
pub(crate) const SPLITTER_GRAB_PX: f32 = 5.0;
/// Width of the band drawn over a divider while it is hovered or dragged. Wider
/// than the 1pt edge it covers, so the divider visibly becomes a handle rather
/// than just changing color, and centered on that edge so nothing shifts.
pub(crate) const SPLITTER_ACTIVE_W: f32 = 3.0;
pub(crate) const SPLITTER_ACTIVE_COLOR: [f32; 4] = [0.45, 0.45, 0.53, 1.0];
/// Floors for the four panels a splitter can squeeze, in points.
///
/// The preview minimum is set by its transport bar rather than the picture:
/// five buttons plus gaps plus the timecode readout is the point below which
/// controls would start overlapping, and a preview can always letterbox.
pub(crate) const POOL_MIN_W: f32 = 160.0;
pub(crate) const PREVIEW_MIN_W: f32 = 340.0;
/// Enough for the transport bar plus a sliver of picture above it.
pub(crate) const TOP_MIN_H: f32 = TRANSPORT_BAR_H + 60.0;
/// Toolbar, ruler and one lane at its minimum height — below this the timeline
/// stops being a timeline.
pub(crate) const TIMELINE_MIN_H: f32 = TIMELINE_TOP_PAD + TIMELINE_RULER_H + TRACK_LANE_MIN_H;

// Surface elevation scale (sRGB), darkest first. Steps widen as they climb:
// down near black a small numeric difference is imperceptible, so the low tiers
// need real distance between them or the whole window reads as one flat mass.
// Every panel picks a tier rather than its own value, so surfaces at the same
// conceptual depth actually match.
pub(crate) const SURFACE_WELL: [f32; 4] = [0.03, 0.03, 0.04, 1.0]; // content the app displays into
pub(crate) const SURFACE_LANE: [f32; 4] = [0.05, 0.05, 0.06, 1.0]; // wells that hold clips
pub(crate) const SURFACE_BASE: [f32; 4] = [0.09, 0.09, 0.11, 1.0]; // body behind the wells
pub(crate) const SURFACE_PANEL: [f32; 4] = [0.15, 0.15, 0.18, 1.0]; // chrome that holds controls

// Panel assignments.
pub(crate) const MEDIA_POOL_COLOR: [f32; 4] = SURFACE_PANEL;
pub(crate) const PREVIEW_COLOR: [f32; 4] = SURFACE_WELL;
/// The canvas itself, sitting inside the preview well. True black rather than
/// the well's near-black: it's picture area, and it has to read as distinct
/// from the panel it floats in even when no clip is playing.
pub(crate) const CANVAS_COLOR: [f32; 4] = [0.0, 0.0, 0.0, 1.0];
pub(crate) const TIMELINE_COLOR: [f32; 4] = SURFACE_BASE;
pub(crate) const LANE_COLOR: [f32; 4] = SURFACE_LANE;
// Edge between two panels, lighter than both — the flat-UI way to define a
// boundary, standing in for the highlight/shadow pair of a bevel.
pub(crate) const PANEL_BORDER_COLOR: [f32; 4] = [0.28, 0.28, 0.33, 1.0];
// Softer line for divisions *within* a panel, which shouldn't read as loudly as
// the panel's own edges.
pub(crate) const DIVIDER_COLOR: [f32; 4] = [0.20, 0.20, 0.24, 1.0];
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
pub(crate) const VIDEO_CLIP_COLOR: [f32; 4] = [0.19, 0.28, 0.44, 1.0];
pub(crate) const AUDIO_CLIP_COLOR: [f32; 4] = [0.19, 0.38, 0.25, 1.0];
// Outline around each clip. Two butt-joined clips show it twice, so a split
// reads as a 2px seam.
pub(crate) const CLIP_BORDER_PX: f32 = 1.0;
pub(crate) const CLIP_BORDER_DARKEN: f32 = 0.40;
// Selection accent. Lighter than the toggle orange so it still separates from
// the saturated blue and green clip fills it has to sit on.
pub(crate) const CLIP_SELECTED_BORDER: [f32; 4] = [1.0, 0.72, 0.30, 1.0];
pub(crate) const CLIP_SELECTED_BORDER_PX: f32 = 2.0;
pub(crate) const CLIP_SELECTED_LIFT: f32 = 0.14;
// How close a dragged edge must come to latch onto a snap target. In pixels
// rather than seconds so the pull feels the same however long the timeline is.
pub(crate) const SNAP_PX: f32 = 8.0;
pub(crate) const AUDIO_WAVE_COLOR: [f32; 4] = [0.75, 0.95, 0.80, 0.95];
pub(crate) const CLIP_LABEL_COLOR: [f32; 4] = [0.95, 0.95, 0.98, 1.0];
pub(crate) const LABEL_COLOR: [f32; 4] = [0.72, 0.72, 0.78, 1.0];
// Was dim enough that V1/A1 were a squint to read; a track's identity should be
// legible at a glance, not decorative.
pub(crate) const TRACK_LABEL_COLOR: [f32; 4] = [0.62, 0.62, 0.68, 1.0];
// The type scale. Every font size below is one of these steps, so a new piece
// of chrome picks a step rather than inventing a value a half point off one
// that already exists.
//
// `TYPE_SM` is the floor: nothing renders text smaller. The ruler labels and the
// pool's format line used to sit a point under it and read as a squint rather
// than as small print, and anything reaching lower is reaching for the same
// mistake. These are points, so the floor holds its physical size across
// displays — see [`crate::state::State::scale`].
pub(crate) const TYPE_SM: f32 = 11.0;
pub(crate) const TYPE_MD: f32 = 12.0;
pub(crate) const TYPE_LG: f32 = 13.0;
pub(crate) const TYPE_XL: f32 = 14.0;
/// Icon glyphs sit above the text steps: a Lucide glyph fills its em box where
/// a letter leaves clearance above and below, so matching a text size optically
/// means exceeding it numerically.
pub(crate) const TYPE_ICON: f32 = 16.0;

pub(crate) const LABEL_SIZE: f32 = TYPE_LG;
pub(crate) const CLIP_LABEL_SIZE: f32 = TYPE_SM;
pub(crate) const LABEL_PAD: f32 = 10.0;
pub(crate) const PLAYHEAD_COLOR: [f32; 4] = [0.95, 0.35, 0.35, 1.0];
pub(crate) const PLAYHEAD_WIDTH: f32 = 2.0;
pub(crate) const TIMER_SIZE: f32 = TYPE_XL;
pub(crate) const TIMER_COLOR: [f32; 4] = [0.95, 0.95, 0.98, 1.0];
// Transport bar between preview and timeline; holds prev/play/next + timer.
pub(crate) const TRANSPORT_BAR_H: f32 = 40.0;
// Panel tier, not well tier: it holds controls, and at its old near-black value
// it dissolved into the preview above it.
pub(crate) const TRANSPORT_BAR_COLOR: [f32; 4] = SURFACE_PANEL;
// Buttons are icon-only, so they no longer need to fit a word — square-ish
// keeps the glyph optically centered and tightens both toolbars considerably.
pub(crate) const TRANSPORT_BTN_W: f32 = 32.0;
pub(crate) const TRANSPORT_BTN_H: f32 = 26.0;
pub(crate) const TRANSPORT_GAP: f32 = 8.0;
pub(crate) const TRANSPORT_ICON_SIZE: f32 = TYPE_ICON;
pub(crate) const TRANSPORT_TOOLTIP_SIZE: f32 = TYPE_SM;

// Status readout, occupying the toolbar row between the edit buttons and the
// right-aligned Export button. Doubles as the progress bar while a render runs
// and the message line the rest of the time, so a render's progress and the
// result of a save never fight for the same space.
pub(crate) const EXPORT_BTN_W: f32 = TRANSPORT_BTN_W;
pub(crate) const EXPORT_READOUT_W: f32 = 190.0;
pub(crate) const EXPORT_READOUT_GAP: f32 = 10.0;
pub(crate) const EXPORT_BAR_H: f32 = 4.0;
pub(crate) const EXPORT_BAR_TRACK: [f32; 4] = [0.10, 0.10, 0.13, 1.0];
pub(crate) const EXPORT_BAR_FILL: [f32; 4] = [0.95, 0.55, 0.15, 1.0];
pub(crate) const STATUS_SIZE: f32 = TYPE_SM;
pub(crate) const STATUS_OK: [f32; 4] = [0.60, 0.85, 0.65, 1.0];
pub(crate) const STATUS_ERR: [f32; 4] = [0.92, 0.55, 0.55, 1.0];
pub(crate) const STATUS_INFO: [f32; 4] = [0.72, 0.72, 0.78, 1.0];
/// How long a status message lingers. Long enough to read, short enough that
/// it clears itself instead of needing a dismiss affordance.
pub(crate) const STATUS_SECONDS: f64 = 8.0;

// Project settings popup.
pub(crate) const MENU_BG: [f32; 4] = [0.13, 0.13, 0.16, 1.0];
pub(crate) const MENU_BORDER: [f32; 4] = [0.34, 0.34, 0.40, 1.0];
pub(crate) const MENU_PAD: f32 = 10.0;
pub(crate) const MENU_ROW_H: f32 = 22.0;
pub(crate) const MENU_ROW_GAP: f32 = 1.0;
pub(crate) const MENU_SECTION_GAP: f32 = 12.0;
pub(crate) const MENU_HEADER_SIZE: f32 = TYPE_SM;
pub(crate) const MENU_HEADER_COLOR: [f32; 4] = [0.62, 0.62, 0.68, 1.0];
pub(crate) const MENU_ROW_SIZE: f32 = TYPE_MD;
pub(crate) const MENU_RES_W: f32 = 132.0;
pub(crate) const MENU_FPS_COL_W: f32 = 60.0;
pub(crate) const MENU_FPS_COL_GAP: f32 = 4.0;
/// Gap between the gear and the popup it opens, and the same faked soft shadow
/// the tooltips use — see the note on `TOOLTIP_SHADOW` in `ui.rs`.
pub(crate) const MENU_GAP: f32 = 7.0;
pub(crate) const MENU_SHADOW: [f32; 4] = [0.0, 0.0, 0.0, 0.10];
pub(crate) const MENU_SHADOW_STEPS: i32 = 3;

// Timeline panel layout.
// Lane height is computed per-frame to fill the timeline area; these bounds
// keep it readable with one track and prevent chunkiness at high counts.
pub(crate) const TRACK_LANE_MIN_H: f32 = 32.0;
pub(crate) const TRACK_LANE_MAX_H: f32 = 88.0;
/// Fraction of the tracks-area height the lanes+gaps try to fill.
pub(crate) const TRACK_LANE_FILL: f32 = 0.9;
pub(crate) const TRACK_LANE_GAP: f32 = 2.0;
pub(crate) const TRACK_HEADER_WIDTH: f32 = 48.0;
// Height of the toolbar band above the ruler, holding the "TIMELINE" label and
// its buttons. Sized as the button height plus even breathing room either side
// rather than hugging it — at the old 30 the 26px buttons cleared the panel
// edge by 2px and looked jammed against it.
pub(crate) const TIMELINE_TOP_PAD: f32 = 46.0;
/// Scrub strip between the title bar and lanes.
pub(crate) const TIMELINE_RULER_H: f32 = 22.0;
pub(crate) const TIMELINE_RULER_COLOR: [f32; 4] = SURFACE_PANEL;
pub(crate) const TIMELINE_RULER_TICK_COLOR: [f32; 4] = [0.58, 0.58, 0.64, 1.0];
pub(crate) const TIMELINE_RULER_LABEL_COLOR: [f32; 4] = [0.72, 0.72, 0.78, 1.0];
pub(crate) const TIMELINE_RULER_LABEL_SIZE: f32 = TYPE_SM;
pub(crate) const TIMELINE_RULER_TICK_H: f32 = 6.0;

// Media pool list layout.
/// Below the MEDIA POOL label.
pub(crate) const POOL_LIST_TOP: f32 = 36.0;
pub(crate) const POOL_ROW_HEIGHT: f32 = 64.0;
pub(crate) const POOL_ROW_GAP: f32 = 4.0;
pub(crate) const POOL_ROW_PAD: f32 = 6.0;
pub(crate) const POOL_ROW_COLOR: [f32; 4] = [0.20, 0.20, 0.24, 1.0];
pub(crate) const POOL_ITEM_NAME_SIZE: f32 = TYPE_MD;
pub(crate) const POOL_ITEM_META_SIZE: f32 = TYPE_SM;
/// Format line under each pool row's filename. Dimmer than the name and a size
/// down, so a row still reads as "a clip called X" at a glance rather than as
/// two competing lines.
pub(crate) const POOL_ITEM_META_COLOR: [f32; 4] = LABEL_COLOR;
pub(crate) const POOL_ITEM_META_GAP: f32 = 5.0;
// Thumbnail slot inside each row — fixed ~16:9 slot, actual thumb is
// letterboxed into it preserving source aspect.
pub(crate) const POOL_THUMB_W: f32 = 92.0;
pub(crate) const POOL_THUMB_H: f32 = POOL_ROW_HEIGHT - POOL_ROW_PAD * 2.0;
pub(crate) const POOL_THUMB_BG: [f32; 4] = [0.08, 0.08, 0.10, 1.0];
pub(crate) const POOL_DUR_BG: [f32; 4] = [0.0, 0.0, 0.0, 0.65];
pub(crate) const POOL_DUR_TEXT: [f32; 4] = [0.95, 0.95, 0.98, 1.0];
/// The close button's hit box, not a font size — hence `BOX`. Every `_SIZE` in
/// this file is a step on the type scale, which is what makes a stray literal
/// on one of them easy to spot.
pub(crate) const POOL_CLOSE_BOX: f32 = 18.0;
pub(crate) const POOL_CLOSE_INSET: f32 = 3.0;
pub(crate) const POOL_CLOSE_BG: [f32; 4] = [0.0, 0.0, 0.0, 0.70];
pub(crate) const POOL_CLOSE_BG_HOVER: [f32; 4] = [0.65, 0.25, 0.25, 0.95];
pub(crate) const POOL_CLOSE_LABEL_SIZE: f32 = TYPE_LG;
pub(crate) const POOL_CLOSE_GLYPH_COLOR: [f32; 4] = [0.95, 0.95, 0.98, 1.0];

// Audio clip level and fades.
//
// The level line is a rubber band drawn across the clip at the height of its
// gain; the fade handles are the two boxes at its top corners. Both are drawn
// over the waveform, so they are lighter than it rather than another shade of
// the clip fill — a line the same weight as a peak would read as one.
/// Kept off the clip's own border so the extremes of the range stay grabbable
/// instead of merging with it.
pub(crate) const CLIP_LEVEL_INSET: f32 = 5.0;
pub(crate) const CLIP_LEVEL_LINE_H: f32 = 1.0;
pub(crate) const CLIP_LEVEL_COLOR: [f32; 4] = [1.0, 0.88, 0.55, 0.55];
/// The band is live only on the selected clip (see `State::timeline_hit`), so
/// the selected line is the one drawn as a handle rather than as a readout.
pub(crate) const CLIP_LEVEL_ACTIVE_COLOR: [f32; 4] = [1.0, 0.82, 0.35, 1.0];
pub(crate) const CLIP_LEVEL_ACTIVE_H: f32 = 2.0;
pub(crate) const CLIP_LEVEL_GRAB_PX: f32 = 5.0;
pub(crate) const CLIP_LEVEL_LABEL_SIZE: f32 = TYPE_SM;
pub(crate) const CLIP_LEVEL_LABEL_COLOR: [f32; 4] = [1.0, 0.90, 0.70, 1.0];
pub(crate) const CLIP_LEVEL_LABEL_PAD: f32 = 5.0;
/// A fade is drawn by shading away the part of the clip it attenuates, so the
/// wedge grows out of the clip rather than being a line laid over it. Black at
/// partial alpha, which darkens fill and waveform together.
pub(crate) const CLIP_FADE_SHADE: [f32; 4] = [0.0, 0.0, 0.0, 0.62];
pub(crate) const CLIP_FADE_EDGE_COLOR: [f32; 4] = [1.0, 0.88, 0.55, 0.85];
pub(crate) const CLIP_FADE_EDGE_H: f32 = 1.0;
pub(crate) const CLIP_FADE_HANDLE_BOX: f32 = 9.0;
pub(crate) const CLIP_FADE_HANDLE_COLOR: [f32; 4] = [1.0, 0.82, 0.35, 0.85];

// Clip interaction.
pub(crate) const CLIP_EDGE_GRAB_PX: f32 = 6.0;
/// The drag ghost is a clip's own fill at reduced alpha. Derived from that fill
/// rather than restated, because these were literal copies of it and would have
/// gone on drawing the old, lower-contrast blue and green after the fills moved.
pub(crate) const DRAG_GHOST_ALPHA: f32 = 0.55;
pub(crate) const DRAG_GHOST_VIDEO_COLOR: [f32; 4] = [
    VIDEO_CLIP_COLOR[0],
    VIDEO_CLIP_COLOR[1],
    VIDEO_CLIP_COLOR[2],
    DRAG_GHOST_ALPHA,
];
pub(crate) const DRAG_GHOST_AUDIO_COLOR: [f32; 4] = [
    AUDIO_CLIP_COLOR[0],
    AUDIO_CLIP_COLOR[1],
    AUDIO_CLIP_COLOR[2],
    DRAG_GHOST_ALPHA,
];

/// Blend `c` toward white by `f`, leaving alpha alone.
pub(crate) fn lighten(c: [f32; 4], f: f32) -> [f32; 4] {
    [
        c[0] + (1.0 - c[0]) * f,
        c[1] + (1.0 - c[1]) * f,
        c[2] + (1.0 - c[2]) * f,
        c[3],
    ]
}

pub(crate) fn darken(c: [f32; 4], f: f32) -> [f32; 4] {
    [c[0] * f, c[1] * f, c[2] * f, c[3]]
}
