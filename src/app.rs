//! The winit side: creating the window, and translating OS events into calls
//! on [`State`]. Nothing here decides anything — the match arms are a keymap,
//! and every arm is one call.

use std::sync::Arc;

use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::ActiveEventLoop,
    keyboard::{KeyCode, PhysicalKey},
    window::{Window, WindowId},
};

use crate::state::State;
use crate::title::caret;

/// Points of touchpad travel that stand in for one detent of a mouse wheel.
/// The only number here that is a judgement rather than a translation: winit
/// reports the two gestures in different units, and something has to say how
/// they compare.
const PIXELS_PER_NOTCH: f32 = 40.0;

#[derive(Default)]
pub(crate) struct App {
    state: Option<State>,
    /// Where dropped files come from on Wayland, which winit does not report
    /// them on. `None` everywhere else, where `DroppedFile` covers it.
    #[cfg(target_os = "linux")]
    wayland_drop: Option<crate::wayland_drop::WaylandDrop>,
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

        #[cfg(target_os = "linux")]
        {
            self.wayland_drop = crate::wayland_drop::WaylandDrop::new(&window);
        }

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
                #[cfg(target_os = "linux")]
                if let Some(drop) = self.wayland_drop.as_mut() {
                    for paths in drop.poll() {
                        state.import_files(&paths);
                    }
                }
                state.poll_disk();
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
                state.cursor_in_window = true;
                state.update_drag();
            }
            WindowEvent::CursorLeft { .. } => {
                state.cursor_in_window = false;
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
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Middle,
                ..
            } => {
                state.begin_pan();
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Middle,
                ..
            } => {
                state.end_pan();
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x, y),
                    // A touchpad reports the distance travelled instead of
                    // detents. Dividing puts a comfortable swipe in the same
                    // range as a few notches of a wheel, so both gestures zoom
                    // at the same rate.
                    MouseScrollDelta::PixelDelta(p) => {
                        (p.x as f32 / PIXELS_PER_NOTCH, p.y as f32 / PIXELS_PER_NOTCH)
                    }
                };
                state.wheel(dx, dy);
            }
            WindowEvent::ModifiersChanged(mods) => {
                state.modifiers = mods.state();
            }
            WindowEvent::KeyboardInput {
                event: KeyEvent { physical_key, state: ElementState::Pressed, repeat, text, .. },
                ..
            } => {
                let PhysicalKey::Code(code) = physical_key else {
                    return;
                };
                let ctrl = state.modifiers.control_key();
                let shift = state.modifiers.shift_key();
                // The colour picker's hex box takes the whole keyboard while
                // it has the caret, as a title's text box does below.
                if state.typing_hex() {
                    match code {
                        KeyCode::Escape | KeyCode::Enter | KeyCode::NumpadEnter => {
                            state.finish_hex_edit()
                        }
                        KeyCode::KeyA if ctrl => state.select_all_hex(),
                        KeyCode::KeyC if ctrl => state.copy_hex(false),
                        KeyCode::KeyX if ctrl => state.copy_hex(true),
                        KeyCode::KeyV if ctrl => state.paste_hex(),
                        KeyCode::Backspace => state.backspace_hex(),
                        KeyCode::Delete => state.delete_hex(),
                        KeyCode::ArrowLeft => state.move_hex_caret(caret::left, shift),
                        KeyCode::ArrowRight => state.move_hex_caret(caret::right, shift),
                        KeyCode::Home | KeyCode::ArrowUp => state.move_hex_caret(caret::home, shift),
                        KeyCode::End | KeyCode::ArrowDown => state.move_hex_caret(caret::end, shift),
                        _ if ctrl => {}
                        _ => {
                            if let Some(text) = text.as_deref() {
                                state.type_hex(text);
                            }
                        }
                    }
                    return;
                }
                // So does naming a style just saved. Enter and Escape both
                // keep the name: the style is saved already, and a name is
                // all there is left to take back.
                if state.typing_style_name() {
                    match code {
                        KeyCode::Escape | KeyCode::Enter | KeyCode::NumpadEnter => {
                            state.finish_style_name()
                        }
                        KeyCode::KeyA if ctrl => state.select_all_in_style_name(),
                        KeyCode::KeyC if ctrl => state.copy_from_style_name(false),
                        KeyCode::KeyX if ctrl => state.copy_from_style_name(true),
                        KeyCode::KeyV if ctrl => state.paste_into_style_name(),
                        KeyCode::Backspace => state.backspace_style_name(),
                        KeyCode::Delete => state.delete_in_style_name(),
                        KeyCode::ArrowLeft => state.move_style_name_caret(caret::left, shift),
                        KeyCode::ArrowRight => state.move_style_name_caret(caret::right, shift),
                        KeyCode::Home | KeyCode::ArrowUp => state.move_style_name_caret(caret::home, shift),
                        KeyCode::End | KeyCode::ArrowDown => state.move_style_name_caret(caret::end, shift),
                        _ if ctrl => {}
                        _ => {
                            if let Some(text) = text.as_deref() {
                                state.type_style_name(text);
                            }
                        }
                    }
                    return;
                }
                // Typing into a title takes the whole keyboard. Every bare
                // letter below is a verb that edits, and they cannot both have
                // the key — so while a title has the caret, nothing else does.
                if state.typing_title() {
                    match code {
                        KeyCode::Escape => state.finish_title_edit(),
                        KeyCode::KeyA if ctrl => state.select_all_in_title(),
                        KeyCode::KeyC if ctrl => state.copy_from_title(false),
                        KeyCode::KeyX if ctrl => state.copy_from_title(true),
                        KeyCode::KeyV if ctrl => state.paste_into_title(),
                        KeyCode::Backspace => state.backspace_title(),
                        KeyCode::Delete => state.delete_in_title(),
                        KeyCode::ArrowLeft => state.move_title_caret(caret::left, shift),
                        KeyCode::ArrowRight => state.move_title_caret(caret::right, shift),
                        KeyCode::ArrowUp => state.move_title_caret(caret::up, shift),
                        KeyCode::ArrowDown => state.move_title_caret(caret::down, shift),
                        KeyCode::Home => state.move_title_caret(caret::home, shift),
                        KeyCode::End => state.move_title_caret(caret::end, shift),
                        // Other Ctrl chords type nothing.
                        _ if ctrl && !matches!(code, KeyCode::Enter | KeyCode::NumpadEnter) => {}
                        // A text box: Enter starts a new line, as in any
                        // box that holds more than one. Done is Esc or a
                        // click anywhere else, and the whole session is one
                        // undo step if it was not what was wanted.
                        KeyCode::Enter | KeyCode::NumpadEnter => state.type_into_title("\n"),
                        // Whatever the layout says this key produces — which is
                        // the only thing that gets accents and other keyboards
                        // right.
                        _ => {
                            if let Some(text) = text.as_deref() {
                                state.type_into_title(text);
                            }
                        }
                    }
                    return;
                }
                match code {
                    // Arrows repeat so holding steps through frames, or walks
                    // through edit points with Shift.
                    KeyCode::ArrowLeft if shift => state.goto_edit_point(false),
                    KeyCode::ArrowRight if shift => state.goto_edit_point(true),
                    KeyCode::ArrowLeft => state.step_frame(-1.0),
                    KeyCode::ArrowRight => state.step_frame(1.0),
                    // Level nudges repeat for the same reason the arrows do:
                    // riding a clip down by 6dB, or a title's opacity down to
                    // a ghost, should be a held key rather than six presses.
                    // Shift is the fine step, matching the way Shift already
                    // qualifies the horizontal arrows.
                    KeyCode::ArrowUp => state.nudge_selected_level(1.0, shift),
                    KeyCode::ArrowDown => state.nudge_selected_level(-1.0, shift),
                    // Undo/redo repeat too — holding Ctrl+Z to walk back
                    // through history is the expected feel.
                    KeyCode::KeyZ if ctrl && shift => state.redo(),
                    KeyCode::KeyZ if ctrl => state.undo(),
                    KeyCode::KeyY if ctrl => state.redo(),
                    // Zoom repeats too — a held key is how you cross several
                    // orders of magnitude of timeline. Both rows of a keyboard
                    // are bound so the numpad's pair works as well as the main
                    // one, and Equal doubles as Plus without the Shift.
                    KeyCode::Minus | KeyCode::NumpadSubtract => state.zoom_at_playhead(-1.0),
                    KeyCode::Equal | KeyCode::NumpadAdd => state.zoom_at_playhead(1.0),
                    // The rest are edge-triggered to avoid repeat spam.
                    _ if repeat => {}
                    // Esc closes whatever is open, the innermost first: a
                    // popup, then a compound clip being edited.
                    KeyCode::Escape => {
                        if state.project_menu_open || state.font_menu.is_some() {
                            state.project_menu_open = false;
                            state.font_menu = None;
                        } else if state.color_picker.is_some() {
                            state.close_color_picker();
                        } else {
                            state.close_compound();
                        }
                    }
                    // Bare letters, like the other clip verbs, with the
                    // Ctrl versions every other program uses kept as well.
                    KeyCode::KeyG => state.combine_selected(),
                    KeyCode::KeyA => state.select_all(),
                    KeyCode::KeyD => state.deselect_all(),
                    // Shift+Z rather than a bare key: it is the counterpart to
                    // the zoom pair above, and every bare letter it could have
                    // had is a verb that edits.
                    KeyCode::KeyZ if shift => state.zoom_timeline_to_fit(),
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
                    KeyCode::KeyC => state.copy_selected(),
                    KeyCode::KeyV => state.paste(),
                    // After Ctrl+E above, which is export.
                    KeyCode::KeyE => state.open_selected_compound(),
                    KeyCode::KeyO if !ctrl => state.open_file_picker(),
                    KeyCode::KeyS if !ctrl => state.split_at_playhead(),
                    KeyCode::KeyF if !ctrl => state.freeze_frame(),
                    KeyCode::KeyR if !ctrl => {
                        state.ripple_enabled = !state.ripple_enabled;
                    }
                    KeyCode::KeyN if !ctrl => {
                        state.snap_enabled = !state.snap_enabled;
                    }
                    KeyCode::KeyT if !ctrl => state.add_title(),
                    // Enter opens the selected clip's text for editing, which
                    // is what Enter does to a name everywhere else.
                    // or a compound clip's contents.
                    KeyCode::Enter | KeyCode::NumpadEnter => {
                        if state.selected_compound().is_some() {
                            state.open_selected_compound();
                        } else {
                            state.edit_selected_title();
                        }
                    }
                    _ => {}
                }
            }
            _ => (),
        }
    }
}
