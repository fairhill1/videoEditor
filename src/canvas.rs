//! The project canvas — the picture every clip is composed onto — together
//! with the settings popup that pins it.
//!
//! One module because the two halves only make sense together: the popup's
//! rows are the settings, and [`State::canvas`] is what those settings resolve
//! to for the preview, the export and frame stepping alike.

use serde::{Deserialize, Serialize};

use crate::fmt::fmt_fps;
use crate::quad::Quad;
use crate::state::State;
use crate::theme::*;
use crate::timeline::{SourceId, TrackKind, Transform};
use crate::ui::{draw_menu_row, Rect};

/// Canvas format used when nothing better is known: an empty timeline, or a
/// source that has vanished from the pool mid-session.
pub(crate) const EXPORT_FALLBACK_SIZE: (u32, u32) = (1920, 1080);
pub(crate) const EXPORT_FALLBACK_FPS: f64 = 30.0;

// Project canvas presets, offered by the settings popup.
pub(crate) const RES_PRESETS: [(u32, u32); 4] =
    [(3840, 2160), (2560, 1440), (1920, 1080), (1280, 720)];
/// Grouped by column as they're laid out: film rates, standard, high. Keeping
/// the grouping in the data means the popup's columns can't drift from the
/// meaning they're supposed to carry.
pub(crate) const FPS_COLUMNS: [&[f64]; 3] = [
    &[23.976, 24.0],
    &[25.0, 29.97, 30.0],
    &[50.0, 59.94, 60.0],
];

/// The project canvas: the picture every clip is composed onto, and the format
/// the export is encoded in.
///
/// This is the frame the preview draws and clips are fitted into, not merely an
/// export setting — when clips gain a position/scale/crop, those are expressed
/// relative to this.
#[derive(Copy, Clone, PartialEq, Debug)]
pub(crate) struct Canvas {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) fps: f64,
}

impl Canvas {
    /// Where a `sw x sh` source lands on the canvas under `transform`, in
    /// canvas pixels.
    ///
    /// The seam every placement goes through. The preview and the export each
    /// draw layers their own way — textured quads on the GPU, scaled planes on
    /// the CPU — but neither works out *where* a picture goes, so the two
    /// cannot drift about it.
    ///
    /// Scaling happens about the fitted rect's own centre, not the canvas's, so
    /// a clip that has been moved into a corner grows in place instead of
    /// crawling back towards the middle as you scale it up.
    pub(crate) fn place(&self, sw: f32, sh: f32, transform: Transform) -> [f32; 4] {
        let (fx, fy, fw, fh) = self.fit(sw, sh);
        let w = fw * transform.scale;
        let h = fh * transform.scale;
        [
            fx + (fw - w) * 0.5 + transform.x * self.width as f32,
            fy + (fh - h) * 0.5 + transform.y * self.height as f32,
            w,
            h,
        ]
    }

    /// Where a `sw x sh` source lands on the canvas with no transform applied:
    /// an aspect-preserving fit, centred.
    pub(crate) fn fit(&self, sw: f32, sh: f32) -> (f32, f32, f32, f32) {
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
pub(crate) enum Setting<T> {
    Auto,
    Fixed(T),
}

/// One clickable row in the settings popup.
#[derive(Copy, Clone, PartialEq, Debug)]
pub(crate) enum ProjectChoice {
    Resolution(Setting<(u32, u32)>),
    Fps(Setting<f64>),
}

impl State {
    /// The video clip whose format `Setting::Auto` follows: earliest on the
    /// timeline, ties broken by the lower track.
    ///
    /// Matching one clip rather than, say, the largest source keeps the common
    /// single-camera timeline a straight passthrough; anything else letterboxes
    /// into it.
    ///
    /// Titles are passed over. A generated source has no format of its own —
    /// it is drawn at whatever the canvas turns out to be — so following one
    /// would mean the canvas following itself, and a project that opened with a
    /// title first would drop to the fallback format instead of the footage's.
    pub(crate) fn reference_video_source(&self) -> Option<SourceId> {
        let mut reference: Option<(f64, usize, SourceId)> = None;
        for (track_idx, track) in self.timeline.tracks.iter().enumerate() {
            if track.kind != TrackKind::Video {
                continue;
            }
            for clip in &track.clips {
                if self.media.get(clip.source).is_none_or(|s| s.stream.is_none()) {
                    continue;
                }
                let better = reference
                    .is_none_or(|(start, ti, _)| (clip.timeline_start, track_idx) < (start, ti));
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
    pub(crate) fn canvas(&self) -> Canvas {
        let reference = self
            .reference_video_source()
            .and_then(|source| self.media.get(source))
            .and_then(|src| src.stream.as_ref());
        let (width, height) = match self.canvas_res {
            Setting::Fixed(size) => size,
            Setting::Auto => reference
                .map(|v| (v.width(), v.height()))
                .unwrap_or(EXPORT_FALLBACK_SIZE),
        };
        let fps = match self.canvas_fps {
            Setting::Fixed(fps) => fps,
            Setting::Auto => reference.map(|v| v.frame_rate()).unwrap_or(0.0),
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

    /// Draw the project settings popup and record where its rows landed.
    ///
    /// Anchored below the gear when there's room and above it when there isn't,
    /// then pulled inside the window — the toolbar sits partway down the
    /// window, so which side has space depends on where the user has dragged
    /// the timeline splitter.
    pub(crate) fn draw_project_menu(&mut self, anchor: Rect, win_w: f32, win_h: f32) {
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
            self.push_menu_row(
                rect,
                &label,
                setting == self.canvas_res,
                ProjectChoice::Resolution(setting),
            );
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
    pub(crate) fn apply_project_choice(&mut self, choice: ProjectChoice) {
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
}
