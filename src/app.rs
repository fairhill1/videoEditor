//! The winit side: creating the window, and translating OS events into calls
//! on [`State`]. Nothing here decides anything — the match arms are a keymap,
//! and every arm is one call.

use std::sync::Arc;

use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::{ElementState, KeyEvent, MouseButton, WindowEvent},
    event_loop::ActiveEventLoop,
    keyboard::{KeyCode, PhysicalKey},
    window::{Window, WindowId},
};

use crate::state::State;

#[derive(Default)]
pub(crate) struct App {
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
