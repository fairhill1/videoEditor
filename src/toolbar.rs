//! The window's chrome: panel labels, the timeline's button row and status
//! readout, and the transport bar under the preview.
//!
//! Each of these both lays a control out and stores its rect on `State`, which
//! is what [`State::begin_drag`] hit-tests against on the next click. Laying
//! out and hit-testing from the same numbers is why a button never sits a few
//! points from where it can be pressed.

use crate::canvas::Canvas;
use crate::fmt::{fmt_fps, format_timecode, truncate_to_width};
use crate::quad::Quad;
use crate::state::State;
use crate::theme::*;
use crate::ui::{draw_button, draw_tooltip, BtnState, Rect, TooltipSide};

impl State {
    /// The "MEDIA POOL" and "PREVIEW" headings, and the pool's import button.
    pub(crate) fn draw_panel_labels(&mut self, w: f32, media_w: f32) {
        let baseline_y = LABEL_PAD + self.text.ascent(LABEL_SIZE);
        self.text.draw(
            &self.queue,
            &mut self.quads,
            [LABEL_PAD, baseline_y],
            "MEDIA POOL",
            LABEL_SIZE,
            LABEL_COLOR,
        );

        // Media pool toolbar: just right of the MEDIA POOL label.
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
    }

    /// The timeline's toolbar band: the label and its buttons share one row,
    /// both centered on the same line, so widening the band moves them down
    /// together instead of drifting apart.
    pub(crate) fn draw_timeline_toolbar(&mut self, w: f32, top_h: f32, canvas: Canvas) {
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

        self.draw_export_readout(toolbar_center_y);
    }

    /// Progress while rendering, result once done. Both share the strip left of
    /// the Export button, so a finished message appears exactly where the bar
    /// that produced it was.
    fn draw_export_readout(&mut self, toolbar_center_y: f32) {
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
                [(readout_right - text_w).round(), block_top + status_ascent],
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
                let message =
                    truncate_to_width(&self.text, &message, STATUS_SIZE, EXPORT_READOUT_W);
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
    }

    /// Prev / play / next centered under the preview, timer right-aligned.
    pub(crate) fn draw_transport_bar(
        &mut self,
        w: f32,
        media_w: f32,
        preview_w: f32,
        preview_h: f32,
        t: f64,
    ) {
        let bar_center_y = preview_h + TRANSPORT_BAR_H * 0.5;
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
        let row_w =
            TRANSPORT_BTN_W * n_transport as f32 + TRANSPORT_GAP * (n_transport - 1) as f32;
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
    }
}
