//! The system clipboard, for text: what Ctrl+C in a text box copies and
//! Ctrl+V pastes, shared with every other program.
//!
//! Kept open for the life of the editor — on X11 the copy lives only as long
//! as the program that made it holds on to it. Where there is no system
//! clipboard to be had, copies stay inside the editor, so copy and paste
//! still work between its own boxes.

use std::sync::{Mutex, OnceLock};

struct Board {
    system: Option<arboard::Clipboard>,
    /// The last copy, for when the system clipboard is missing or refuses.
    own: String,
}

fn board() -> &'static Mutex<Board> {
    static BOARD: OnceLock<Mutex<Board>> = OnceLock::new();
    BOARD.get_or_init(|| {
        let system = arboard::Clipboard::new()
            .map_err(|e| log::warn!("no system clipboard, copies stay in the editor: {e}"))
            .ok();
        Mutex::new(Board { system, own: String::new() })
    })
}

pub fn copy(text: &str) {
    if text.is_empty() {
        return;
    }
    let mut board = board().lock().unwrap();
    board.own = text.to_string();
    if let Some(system) = board.system.as_mut() {
        if let Err(e) = system.set_text(text.to_string()) {
            log::warn!("could not copy to the system clipboard: {e}");
        }
    }
}

pub fn paste() -> String {
    let mut board = board().lock().unwrap();
    match board.system.as_mut().map(|s| s.get_text()) {
        Some(Ok(text)) => text,
        _ => board.own.clone(),
    }
}
