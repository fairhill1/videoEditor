//! A video editor.
//!
//! The modules divide along two axes. Down one are the media layers, each
//! owning a resource and knowing nothing of the UI: `video` and `audio` decode,
//! `media` pools what they produce, `timeline` is the edit model, `compose`
//! resolves it into the pictures one frame is made of, `project` is its file
//! format, and `export` renders it out.
//!
//! Along the other is the UI, which is one big [`state::State`] with its
//! behaviour split by concern rather than by data. `theme` holds the constants
//! everything draws with; `layout` answers where things are; `input` turns
//! gestures into edits and `edit` applies them; `session` moves the project to
//! and from disk; `canvas` resolves the project format; and `render`,
//! `toolbar` and `timeline_view` draw a frame. `quad`, `text` and `ui` are the
//! primitives all of that draws through.

mod app;
mod audio;
mod canvas;
mod compose;
mod edit;
mod export;
mod fmt;
mod input;
mod layout;
mod media;
mod project;
mod quad;
mod render;
mod session;
mod state;
mod text;
mod theme;
mod timeline;
mod timeline_view;
mod title;
mod toolbar;
mod ui;
mod video;

use winit::event_loop::{ControlFlow, EventLoop};

use app::App;

fn main() {
    env_logger::init();

    let event_loop = EventLoop::new().unwrap();
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut app = App::default();
    event_loop.run_app(&mut app).unwrap();
}
