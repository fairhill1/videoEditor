//! A video editor.
//!
//! The modules divide along two axes. Down one are the media layers, each
//! owning a resource and knowing nothing of the UI: `video` and `audio` decode,
//! `media` pools what they produce, `timeline` is the edit model, `compose`
//! resolves it into the pictures one frame is made of, `project` is its file
//! format, and `export` renders it out. `colorspace` is the RGB/YUV matrix
//! the decode and the export both convert with.
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
mod blur;
mod canvas;
mod clipboard;
mod colorspace;
mod compose;
mod edit;
mod effects;
mod effects_view;
mod export;
mod filmstrip;
mod fmt;
mod color;
mod compound;
mod picker;
mod fonts;
mod freeze;
mod input;
mod layout;
mod media;
mod project;
mod quad;
mod reload;
mod render;
mod session;
mod state;
mod still;
mod text;
mod textedit;
mod theme;
mod timeline;
mod timeline_view;
mod title;
mod toolbar;
mod ui;
mod video;
#[cfg(target_os = "linux")]
mod wayland_drop;

use winit::event_loop::{ControlFlow, EventLoop};

use app::App;

fn main() {
    env_logger::init();

    // `videoEditor --check edit.vedit`: say whether a project file would
    // open cleanly, without opening a window.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--check") {
        let Some(path) = args.get(1) else {
            eprintln!("usage: videoEditor --check <project.vedit>");
            std::process::exit(2);
        };
        match reload::check(std::path::Path::new(path)) {
            Ok(summary) => println!("{summary}"),
            Err(problems) => {
                eprintln!("{problems}");
                std::process::exit(1);
            }
        }
        return;
    }

    // `videoEditor --export edit.vedit out.mp4`: render a project file
    // without opening a window.
    if args.first().map(String::as_str) == Some("--export") {
        let (Some(project), Some(output)) = (args.get(1), args.get(2)) else {
            eprintln!("usage: videoEditor --export <project.vedit> <out.mp4>");
            std::process::exit(2);
        };
        if let Err(e) = export::headless::export(std::path::Path::new(project), std::path::Path::new(output)) {
            eprintln!("export failed: {e}");
            std::process::exit(1);
        }
        return;
    }

    let event_loop = EventLoop::new().unwrap();
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut app = App::default();
    event_loop.run_app(&mut app).unwrap();
}
