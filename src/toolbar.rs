//! The window's chrome: panel labels, the timeline's button row and status
//! readout, and the transport bar under the preview.
//!
//! Each of these both lays a control out and stores its rect on `State`, which
//! is what [`State::begin_drag`] hit-tests against on the next click. Laying
//! out and hit-testing from the same numbers is why a button never sits a few
//! points from where it can be pressed.

use crate::canvas::Canvas;
use crate::fmt::{fmt_fps, format_timecode, truncate_to_width};
use crate::input::DragMode;
use crate::layout::{zoom_fraction, PoolTab, PoolView};
use crate::quad::Quad;
use crate::state::State;
use crate::theme::*;
use crate::ui::{draw_button, draw_slider, draw_tooltip, BtnState, Rect, TooltipSide};

impl State {
    /// The pool panel's MEDIA and EFFECTS tabs, the PREVIEW heading, and the
    /// buttons of whichever pool page is showing.
    pub(crate) fn draw_panel_labels(&mut self, w: f32, media_w: f32) {
        let baseline_y = LABEL_PAD + self.text.ascent(LABEL_SIZE);
        let tabs_right = self.draw_pool_tabs(baseline_y);

        self.text.draw(
            &self.queue,
            &mut self.quads,
            [media_w + LABEL_PAD, baseline_y],
            "PREVIEW",
            LABEL_SIZE,
            LABEL_COLOR,
        );
        if self.pool_tab != PoolTab::Media {
            return;
        }

        // Media pool toolbar: just right of the tabs.
        self.pool_open_btn = Rect {
            x: (tabs_right + LABEL_PAD * 1.5).round(),
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

        // Beside the import button, because both answer "put something new in
        // the pool" — one from disk and one out of nothing.
        self.pool_title_btn = Rect {
            x: (self.pool_open_btn.x + TRANSPORT_BTN_W + TRANSPORT_GAP).round(),
            ..self.pool_open_btn
        };
        let pool_title_hovered = self.pool_title_btn.contains(self.cursor);
        draw_button(
            &mut self.quads,
            &mut self.text,
            &self.queue,
            self.pool_title_btn,
            // A letter from the text face rather than a pictogram: the icon
            // font is a subset built for this UI, and a 'T' says "text" more
            // plainly than anything that would have to be added to it.
            'T',
            TRANSPORT_ICON_SIZE,
            pool_title_hovered,
            BtnState::Normal,
        );
        if pool_title_hovered {
            draw_tooltip(
                &mut self.quads,
                &mut self.text,
                &self.queue,
                self.pool_title_btn,
                "New title (T)",
                TRANSPORT_TOOLTIP_SIZE,
                TooltipSide::Below,
                w,
            );
        }
        // Against the pool's right edge, apart from the two that add things:
        // this one changes how the pool looks rather than what is in it.
        self.pool_view_btn = Rect {
            x: (media_w - LABEL_PAD - TRANSPORT_BTN_W).round(),
            ..self.pool_open_btn
        };
        let pool_view_hovered = self.pool_view_btn.contains(self.cursor);
        // The icon shows the view a press switches to, as the tooltip says.
        let (view_icon, view_tip) = match self.pool_view {
            PoolView::List => (ICON_GRID, "Show as grid"),
            PoolView::Grid => (ICON_LIST, "Show as list"),
        };
        draw_button(
            &mut self.quads,
            &mut self.text,
            &self.queue,
            self.pool_view_btn,
            view_icon,
            TRANSPORT_ICON_SIZE,
            pool_view_hovered,
            BtnState::Normal,
        );
        if pool_view_hovered {
            draw_tooltip(
                &mut self.quads,
                &mut self.text,
                &self.queue,
                self.pool_view_btn,
                view_tip,
                TRANSPORT_TOOLTIP_SIZE,
                TooltipSide::Below,
                w,
            );
        }
    }

    /// MEDIA and EFFECTS, as the pool panel's heading: the page showing in
    /// full strength with the accent under it, the other dimmed until the
    /// cursor is on it. Returns where the last tab ends.
    fn draw_pool_tabs(&mut self, baseline_y: f32) -> f32 {
        let mark_y = (baseline_y + self.text.descent(LABEL_SIZE)).round() + 1.0;
        let mut x = LABEL_PAD;
        for (i, (label, tab)) in [("MEDIA", PoolTab::Media), ("EFFECTS", PoolTab::Effects)]
            .into_iter()
            .enumerate()
        {
            let tw = self.text.measure_width(label, LABEL_SIZE);
            // The hit box takes in the gap around the word, so the tabs are
            // not a pair of thin targets with dead space between them.
            let rect = Rect {
                x: x - POOL_TAB_GAP * 0.5,
                y: 0.0,
                w: tw + POOL_TAB_GAP,
                h: POOL_LIST_TOP - EFFECT_ROW_GAP,
            };
            self.pool_tab_btns[i] = rect;
            let active = self.pool_tab == tab;
            let color = if active {
                POOL_TAB_ACTIVE_COLOR
            } else if rect.contains(self.cursor) && matches!(self.drag, DragMode::None) {
                POOL_TAB_HOVER_COLOR
            } else {
                POOL_TAB_IDLE_COLOR
            };
            self.text.draw(&self.queue, &mut self.quads, [x, baseline_y], label, LABEL_SIZE, color);
            if active {
                self.quads.push(Quad::colored(
                    [x, mark_y],
                    [tw.round(), POOL_TAB_MARK_H],
                    POOL_TAB_MARK_COLOR,
                ));
            }
            x += tw + POOL_TAB_GAP;
        }
        x - POOL_TAB_GAP
    }

    /// The timeline's toolbar band: the label and its buttons share one row,
    /// both centered on the same line, so widening the band moves them down
    /// together instead of drifting apart.
    pub(crate) fn draw_timeline_toolbar(&mut self, w: f32, top_h: f32, canvas: Canvas) {
        let toolbar_center_y = top_h + TIMELINE_TOP_PAD * 0.5;
        let btn_y = (toolbar_center_y - TRANSPORT_BTN_H * 0.5).round();
        // Inside a compound clip: a back button, then the compound clip's
        // name where the timeline's label goes, so it is plain which timeline
        // this is and how to leave it.
        let compound_name = self
            .editing_compound()
            .and_then(|id| self.media.get(id))
            .map(|src| src.name.to_uppercase());
        let mut label_x = LABEL_PAD;
        self.timeline_back_btn = Rect::default();
        if compound_name.is_some() {
            self.timeline_back_btn =
                Rect { x: LABEL_PAD, y: btn_y, w: TRANSPORT_BTN_W, h: TRANSPORT_BTN_H };
            label_x += TRANSPORT_BTN_W + TRANSPORT_GAP * 2.0;
        }
        let (label, label_color) = match compound_name.as_deref() {
            Some(name) => (name, COMPOUND_LABEL_COLOR),
            None => ("TIMELINE", LABEL_COLOR),
        };
        let label = truncate_to_width(&self.text, label, LABEL_SIZE, COMPOUND_LABEL_MAX_W);
        self.text.draw(
            &self.queue,
            &mut self.quads,
            [
                label_x,
                (toolbar_center_y + self.text.ascent(LABEL_SIZE) * 0.5).round(),
            ],
            &label,
            LABEL_SIZE,
            label_color,
        );

        let timeline_label_w = self.text.measure_width(&label, LABEL_SIZE);
        let first_x = (label_x + timeline_label_w + LABEL_PAD * 1.5).round();
        let stride = TRANSPORT_BTN_W + TRANSPORT_GAP;
        // Select all heads the clip buttons: it picks what the rest act on.
        self.timeline_select_all_btn = Rect {
            x: first_x,
            y: btn_y,
            w: TRANSPORT_BTN_W,
            h: TRANSPORT_BTN_H,
        };
        let btn_x = first_x + stride;
        self.timeline_split_btn = Rect { x: btn_x, ..self.timeline_select_all_btn };
        // Freeze beside Split: both cut the clip under the playhead, and
        // this one holds its picture there before playing on.
        self.timeline_freeze_btn = Rect {
            x: btn_x + stride,
            ..self.timeline_split_btn
        };
        // Delete follows — it acts on clips too — with undo/redo after them.
        // Those live here rather than in a window-level toolbar because
        // history covers timeline + pool edits, which is what this strip
        // governs.
        self.timeline_delete_btn = Rect {
            x: btn_x + stride * 2.0,
            ..self.timeline_split_btn
        };
        self.timeline_copy_btn = Rect {
            x: btn_x + stride * 3.0,
            ..self.timeline_split_btn
        };
        self.timeline_paste_btn = Rect {
            x: btn_x + stride * 4.0,
            ..self.timeline_split_btn
        };
        self.timeline_combine_btn = Rect {
            x: btn_x + stride * 5.0,
            ..self.timeline_split_btn
        };
        self.timeline_open_compound_btn = Rect {
            x: btn_x + stride * 6.0,
            ..self.timeline_split_btn
        };
        self.timeline_undo_btn = Rect {
            x: btn_x + stride * 7.0,
            ..self.timeline_split_btn
        };
        self.timeline_redo_btn = Rect {
            x: btn_x + stride * 8.0,
            ..self.timeline_split_btn
        };
        self.timeline_snap_btn = Rect {
            x: btn_x + stride * 9.0,
            ..self.timeline_split_btn
        };
        // Beside the magnet: both change what a drag on the timeline does
        // rather than doing anything themselves.
        self.timeline_ripple_btn = Rect {
            x: btn_x + stride * 10.0,
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
        // Reload, before Open: the file again, as it is on disk now.
        self.timeline_reload_btn = Rect {
            x: (self.timeline_open_btn.x - TRANSPORT_GAP - TRANSPORT_BTN_W).round(),
            ..self.timeline_project_btn
        };
        // Zoom, after the editing buttons and set a little apart from them:
        // out, the slider, in, then fit. Every way the keyboard and wheel have
        // of changing the view has a control here too. Left out altogether
        // when the window is too narrow to fit them before the export readout,
        // rather than drawn underneath it.
        let zoom_x = self.timeline_ripple_btn.x + stride + TRANSPORT_GAP;
        let zoom_w = TRANSPORT_BTN_W * 3.0 + ZOOM_SLIDER_W + TRANSPORT_GAP * 3.0;
        let readout_left = self.timeline_reload_btn.x - EXPORT_READOUT_GAP - EXPORT_READOUT_W;
        if zoom_x + zoom_w <= readout_left {
            self.timeline_zoom_out_btn = Rect { x: zoom_x, ..self.timeline_split_btn };
            self.timeline_zoom_slider = Rect {
                x: zoom_x + TRANSPORT_BTN_W + TRANSPORT_GAP,
                w: ZOOM_SLIDER_W,
                ..self.timeline_split_btn
            };
            self.timeline_zoom_in_btn = Rect {
                x: self.timeline_zoom_slider.x + ZOOM_SLIDER_W + TRANSPORT_GAP,
                ..self.timeline_split_btn
            };
            self.timeline_zoom_fit_btn = Rect {
                x: self.timeline_zoom_in_btn.x + stride,
                ..self.timeline_split_btn
            };
        } else {
            self.timeline_zoom_out_btn = Rect::default();
            self.timeline_zoom_slider = Rect::default();
            self.timeline_zoom_in_btn = Rect::default();
            self.timeline_zoom_fit_btn = Rect::default();
        }
        let (_, view_dur) = self.view_window();
        let zoom = zoom_fraction(view_dur, self.content_duration());
        let fitted = view_dur >= self.content_duration();

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
                self.timeline_reload_btn,
                ICON_RELOAD,
                if self.disk.pending {
                    "Take the change made on disk (Ctrl+Z brings yours back)"
                } else {
                    "Reload the project from disk"
                },
                if self.disk.pending {
                    BtnState::Toggle(true)
                } else {
                    avail(self.project_path.is_some())
                },
            ),
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
                self.timeline_select_all_btn,
                ICON_SELECT_ALL,
                "Select every clip (A)",
                avail(self.timeline.tracks.iter().any(|t| !t.clips.is_empty())),
            ),
            (
                self.timeline_split_btn,
                ICON_SPLIT,
                "Split selected clips at playhead (S)",
                avail(self.has_selection()),
            ),
            (
                self.timeline_freeze_btn,
                ICON_FREEZE,
                "Freeze the frame at the playhead for 2 seconds (F)",
                avail(self.freeze_target().is_some()),
            ),
            (
                self.timeline_delete_btn,
                ICON_DELETE,
                "Delete selected clips (Del)",
                avail(self.has_selection()),
            ),
            (
                self.timeline_copy_btn,
                ICON_COPY,
                "Copy selected clips (C)",
                avail(self.has_selection()),
            ),
            (
                self.timeline_paste_btn,
                ICON_PASTE,
                "Paste at the playhead (V)",
                avail(!self.clipboard.is_empty()),
            ),
            (
                self.timeline_combine_btn,
                ICON_COMBINE,
                "Combine selected clips into a compound clip (G)",
                avail(self.has_selection()),
            ),
            (
                self.timeline_open_compound_btn,
                ICON_EDIT,
                "Open the selected compound clip to edit it (E)",
                avail(self.selected_compound().is_some()),
            ),
            (
                self.timeline_back_btn,
                ICON_BACK,
                "Back to the timeline this compound clip is on (Esc)",
                BtnState::Normal,
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
                self.timeline_ripple_btn,
                ICON_RIPPLE,
                "Ripple trims: what comes after a clip moves with its end (R)",
                BtnState::Toggle(self.ripple_enabled),
            ),
            (
                self.timeline_zoom_out_btn,
                ICON_ZOOM_OUT,
                "Zoom out (-)",
                avail(!fitted),
            ),
            (
                self.timeline_zoom_in_btn,
                ICON_ZOOM_IN,
                "Zoom in (=)",
                avail(zoom < 1.0),
            ),
            (
                self.timeline_zoom_fit_btn,
                ICON_ZOOM_FIT,
                "Fit the whole edit (Shift+Z)",
                avail(!fitted),
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
            // A button squeezed out of a narrow window has no rect to draw.
            if rect.w <= 0.0 {
                continue;
            }
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

        self.draw_zoom_slider(zoom, w);
        self.draw_export_readout(toolbar_center_y);
    }

    /// The zoom slider at `zoom` along it (see [`zoom_fraction`]): a groove
    /// with the knob in it, the stretch left of the knob filled in so how far
    /// in you are reads at a glance.
    fn draw_zoom_slider(&mut self, zoom: f64, w: f32) {
        let slider = self.timeline_zoom_slider;
        if slider.w <= 0.0 {
            return;
        }
        let hovered = slider.contains(self.cursor);
        let knob_color = match self.drag {
            DragMode::ZoomSlider => SCROLLBAR_THUMB_ACTIVE_COLOR,
            DragMode::None if hovered => SCROLLBAR_THUMB_HOVER_COLOR,
            _ => SCROLLBAR_THUMB_COLOR,
        };
        draw_slider(&mut self.quads, slider, zoom as f32, knob_color);
        if hovered && matches!(self.drag, DragMode::None) {
            draw_tooltip(
                &mut self.quads,
                &mut self.text,
                &self.queue,
                slider,
                "Zoom (scroll over the timeline)",
                TRANSPORT_TOOLTIP_SIZE,
                TooltipSide::Below,
                w,
            );
        }
    }

    /// Progress while rendering, result once done. Both share the strip left of
    /// the Export button, so a finished message appears exactly where the bar
    /// that produced it was.
    fn draw_export_readout(&mut self, toolbar_center_y: f32) {
        let readout_right = self.timeline_reload_btn.x - EXPORT_READOUT_GAP;
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
