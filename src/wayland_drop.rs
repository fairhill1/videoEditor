//! Files dragged in from a file manager, on Wayland.
//!
//! winit reports dropped files on X11, Windows and macOS, but its Wayland
//! backend has no drag and drop at all, so on a Wayland session a file dragged
//! out of Dolphin or Nautilus never reached the app. This listens for drops
//! itself, on winit's own connection to the compositor with an event queue of
//! its own, and hands the paths back for [`crate::state::State::import_files`].
//!
//! The protocol: every drag that crosses the window arrives as a
//! `wl_data_offer`, which lists the formats the source can provide. Accepting
//! `text/uri-list` on enter is what tells the compositor this window takes the
//! drop; on drop the URI list is read out of a pipe the source writes into.
//! That read happens on a thread, so a slow source cannot stall the window.

use std::io::Read;
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};

use wayland_client::backend::Backend;
use wayland_client::protocol::wl_data_device::{self, WlDataDevice};
use wayland_client::protocol::wl_data_device_manager::{self, DndAction, WlDataDeviceManager};
use wayland_client::protocol::wl_data_offer::{self, WlDataOffer};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::{event_created_child, Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use winit::raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
use winit::window::Window;

/// The one format a file manager offers that names files rather than carrying
/// their contents.
const URI_LIST: &str = "text/uri-list";

/// A listener for file drops, alive as long as the window is.
pub(crate) struct WaylandDrop {
    conn: Connection,
    queue: EventQueue<Listener>,
    listener: Listener,
    dropped: Receiver<Vec<String>>,
}

impl WaylandDrop {
    /// Start listening on `window`'s connection. `None` when the window is not
    /// on Wayland — X11 gets its drops from winit — or the compositor has no
    /// drag and drop to offer.
    pub(crate) fn new(window: &Window) -> Option<Self> {
        let RawDisplayHandle::Wayland(handle) = window.display_handle().ok()?.as_raw() else {
            return None;
        };
        // SAFETY: the display is winit's, and outlives this listener because
        // the window it belongs to does: both live as long as the app.
        let backend = unsafe { Backend::from_foreign_display(handle.display.as_ptr().cast()) };
        let conn = Connection::from_backend(backend);
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());

        let (tx, dropped) = mpsc::channel();
        let mut listener = Listener {
            conn: conn.clone(),
            manager: None,
            seats: Vec::new(),
            devices: Vec::new(),
            offers: Vec::new(),
            current: None,
            tx,
        };
        // Once to learn the globals, once more for the devices made from them.
        queue.roundtrip(&mut listener).ok()?;
        queue.roundtrip(&mut listener).ok()?;
        if listener.manager.is_none() {
            log::warn!("the compositor offers no drag and drop; dropped files will be ignored");
            return None;
        }
        Some(Self { conn, queue, listener, dropped })
    }

    /// Handle whatever the compositor has said since the last call, and return
    /// the files dropped since then, a list per drop. Called once a frame; it
    /// does not block.
    pub(crate) fn poll(&mut self) -> Vec<Vec<String>> {
        // winit reads the socket for every queue on the connection, so the
        // events are already here; this only runs their handlers.
        if let Err(e) = self.queue.dispatch_pending(&mut self.listener) {
            log::warn!("drag and drop listener: {e}");
        }
        let _ = self.conn.flush();
        self.dropped.try_iter().collect()
    }
}

/// The state the event handlers share.
struct Listener {
    conn: Connection,
    manager: Option<WlDataDeviceManager>,
    seats: Vec<WlSeat>,
    devices: Vec<WlDataDevice>,
    /// Every offer the compositor has introduced and not yet retired, with
    /// whether it can give file URIs. Clipboard contents arrive as offers too,
    /// and are retired as soon as they do.
    offers: Vec<(WlDataOffer, bool)>,
    /// The drag over the window right now, if any.
    current: Option<WlDataOffer>,
    tx: Sender<Vec<String>>,
}

impl Listener {
    /// A data device for every seat, made as soon as both a seat and the
    /// manager exist, whichever arrived first.
    fn attach_devices(&mut self, qh: &QueueHandle<Self>) {
        let Some(manager) = self.manager.as_ref() else {
            return;
        };
        for seat in &self.seats {
            if !self.devices.iter().any(|d| d.data::<WlSeat>() == Some(seat)) {
                self.devices.push(manager.get_data_device(seat, qh, seat.clone()));
            }
        }
    }

    fn offers_files(&self, offer: &WlDataOffer) -> bool {
        self.offers.iter().any(|(o, files)| o == offer && *files)
    }

    /// Forget `offer` and tell the compositor it can go.
    fn retire(&mut self, offer: &WlDataOffer) {
        self.offers.retain(|(o, _)| o != offer);
        offer.destroy();
    }
}

impl Dispatch<WlRegistry, ()> for Listener {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global { name, interface, version } = event else {
            return;
        };
        match interface.as_str() {
            "wl_data_device_manager" if state.manager.is_none() => {
                // Version 3 is where drags gained actions and `finish`, which
                // is what lets the source know the drop was taken.
                state.manager = Some(registry.bind(name, version.min(3), qh, ()));
                state.attach_devices(qh);
            }
            "wl_seat" => {
                state.seats.push(registry.bind(name, version.min(5), qh, ()));
                state.attach_devices(qh);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlSeat, ()> for Listener {
    fn event(_: &mut Self, _: &WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlDataDeviceManager, ()> for Listener {
    fn event(
        _: &mut Self,
        _: &WlDataDeviceManager,
        _: wl_data_device_manager::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlDataDevice, WlSeat> for Listener {
    fn event(
        state: &mut Self,
        _: &WlDataDevice,
        event: wl_data_device::Event,
        _: &WlSeat,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { id } => state.offers.push((id, false)),
            wl_data_device::Event::Enter { serial, id, .. } => {
                if let Some(old) = state.current.take() {
                    state.retire(&old);
                }
                let Some(offer) = id else {
                    return;
                };
                // Accepting a format is what makes the window a drop target;
                // declining leaves the drag showing "no drop here".
                if state.offers_files(&offer) {
                    offer.accept(serial, Some(URI_LIST.to_string()));
                    if offer.version() >= 3 {
                        offer.set_actions(DndAction::Copy, DndAction::Copy);
                    }
                } else {
                    offer.accept(serial, None);
                }
                state.current = Some(offer);
            }
            wl_data_device::Event::Leave => {
                if let Some(offer) = state.current.take() {
                    state.retire(&offer);
                }
            }
            wl_data_device::Event::Drop => {
                let Some(offer) = state.current.take() else {
                    return;
                };
                if !state.offers_files(&offer) {
                    state.retire(&offer);
                    return;
                }
                state.offers.retain(|(o, _)| o != &offer);
                receive(&state.conn, offer, state.tx.clone());
            }
            // The clipboard, which this has no use for.
            wl_data_device::Event::Selection { id: Some(offer) } => state.retire(&offer),
            _ => {}
        }
    }

    event_created_child!(Listener, WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, ()),
    ]);
}

impl Dispatch<WlDataOffer, ()> for Listener {
    fn event(
        state: &mut Self,
        offer: &WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            if mime_type == URI_LIST {
                if let Some(entry) = state.offers.iter_mut().find(|(o, _)| o == offer) {
                    entry.1 = true;
                }
            }
        }
    }
}

/// Ask the source for `offer`'s URI list and read it on a thread, sending the
/// local paths in it down `tx` and then telling the source the drop is done.
fn receive(conn: &Connection, offer: WlDataOffer, tx: Sender<Vec<String>>) {
    let (mut reader, writer) = match std::io::pipe() {
        Ok(pipe) => pipe,
        Err(e) => {
            log::warn!("could not open a pipe for a dropped file: {e}");
            offer.destroy();
            return;
        }
    };
    offer.receive(URI_LIST.to_string(), writer.as_fd());
    // The source holds its own copy of the write end now. Closing ours is what
    // lets the read below see the end of the list once the source closes its.
    drop(writer);
    let _ = conn.flush();
    let conn = conn.clone();
    let spawned = std::thread::Builder::new().name("file-drop".into()).spawn(move || {
        let mut bytes = Vec::new();
        if let Err(e) = reader.read_to_end(&mut bytes) {
            log::warn!("reading a dropped file list failed: {e}");
        }
        if offer.version() >= 3 {
            offer.finish();
        }
        offer.destroy();
        let _ = conn.flush();
        let paths = uri_list_paths(&bytes);
        if !paths.is_empty() {
            let _ = tx.send(paths);
        }
    });
    if let Err(e) = spawned {
        log::warn!("could not start reading a dropped file list: {e}");
    }
}

/// The local file paths in a `text/uri-list`: one URI a line, `#` lines being
/// comments. Anything that is not a `file:` URI, or that does not decode to a
/// UTF-8 path, is left out, since the importer works in `&str`.
fn uri_list_paths(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|uri| uri.strip_prefix("file://"))
        // `file:///home/...` has an empty host; `file://host/home/...` names
        // one. Either way the path starts at the first slash.
        .filter_map(|rest| rest.find('/').map(|at| &rest[at..]))
        .filter_map(|path| {
            let decoded = percent_decode(path)?;
            PathBuf::from(std::ffi::OsStr::from_bytes(&decoded)).to_str().map(str::to_string)
        })
        .collect()
}

/// `%XX` escapes turned back into the bytes they stand for. `None` for a
/// malformed escape rather than a guess at what was meant.
fn percent_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_manager_uri_list_becomes_paths() {
        let list = b"file:///home/me/Videos/clip%20one.mp4\r\nfile:///home/me/a.png\r\n";
        assert_eq!(
            uri_list_paths(list),
            vec!["/home/me/Videos/clip one.mp4", "/home/me/a.png"]
        );
    }

    #[test]
    fn comments_blank_lines_and_other_schemes_are_skipped() {
        let list = b"# from a file manager\r\n\r\nhttps://example.com/x.mp4\r\nfile:///ok.wav\r\n";
        assert_eq!(uri_list_paths(list), vec!["/ok.wav"]);
    }

    #[test]
    fn a_named_host_is_stepped_over() {
        assert_eq!(uri_list_paths(b"file://cachy/home/me/x.mp4"), vec!["/home/me/x.mp4"]);
    }

    #[test]
    fn escapes_decode_to_utf8_and_bad_ones_drop_the_line() {
        assert_eq!(uri_list_paths(b"file:///v/%C3%A6.mp4"), vec!["/v/\u{e6}.mp4"]);
        assert!(uri_list_paths(b"file:///v/%Z1.mp4").is_empty());
        assert!(uri_list_paths(b"file:///v/%4").is_empty());
    }
}
