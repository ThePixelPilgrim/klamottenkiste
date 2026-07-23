//! SPIKE SCOPE: a minimal, TEXT-ONLY clipboard bridge between the nested seat's
//! `wl_data_device` selection and whatever host clipboard the embedder owns.
//!
//! This is a de-risking proof that selections can cross the nested<->host boundary,
//! NOT a production clipboard. Deliberate limitations:
//!
//! - clipboard selection only (no primary selection),
//! - text only (`text/plain;charset=utf-8`, `text/plain`, `UTF8_STRING`),
//! - no drag-and-drop changes (server DnD keeps its own path in `crate::dnd`),
//! - transfers are read/written whole into memory on short-lived threads.
//!
//! THREAD MODEL. The `SelectionHandler` callbacks run on the COMPOSITOR thread
//! (inside the calloop event loop). The host clipboard (`gdk::Clipboard` in the GTK
//! host) lives on the GTK MAIN thread. The two never touch each other's objects:
//! they exchange plain `String`s over two crossbeam channels held in
//! `ClipboardBridge`, and the compositor side is serviced once per event-loop
//! iteration by `service`.
//!
//! Reading a client selection means asking the client to write into a pipe, and
//! writing our selection means the client reads from a pipe. Neither may block the
//! compositor event loop (the client can only make progress once we return and the
//! display is flushed), so both transfers happen on short-lived `std::thread`s.

use std::{
    fs::File,
    io::{Read, Write},
    os::fd::OwnedFd,
    sync::{Arc, Mutex},
};

use crossbeam_channel::{Receiver, Sender};

use smithay::wayland::selection::{
    data_device::{request_data_device_client_selection, set_data_device_selection},
    SelectionSource,
};

use tracing::{debug, warn};

use crate::state::Compositor;

/// The mime types this spike bridge understands, in the order it prefers them.
pub const TEXT_MIME_TYPES: [&str; 3] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"];

/// The mime types the bridge advertises when it offers the host clipboard to the client.
pub fn text_mime_types() -> Vec<String> {
    TEXT_MIME_TYPES.iter().map(|mime| (*mime).to_string()).collect()
}

/// Pick the best text mime type out of the ones a selection source offers.
///
/// Matching is ASCII-case-insensitive and ignores spaces around the parameter
/// separator, because clients spell `text/plain; charset=utf-8` several ways.
/// Returns the mime type spelled exactly as the source offered it, since that is the
/// string the source expects back in its `send` request.
pub fn preferred_text_mime(offered: &[String]) -> Option<String> {
    for wanted in TEXT_MIME_TYPES {
        for candidate in offered {
            if normalise_mime(candidate) == normalise_mime(wanted) {
                return Some(candidate.clone());
            }
        }
    }
    None
}

/// Lowercase a mime type and drop spaces so `text/plain; charset=UTF-8` compares equal
/// to `text/plain;charset=utf-8`.
fn normalise_mime(mime: &str) -> String {
    mime.chars()
        .filter(|character| !character.is_whitespace())
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

/// Whether a mime type the client asked us to serve is one of our text types.
pub fn is_text_mime(mime: &str) -> bool {
    TEXT_MIME_TYPES
        .iter()
        .any(|wanted| normalise_mime(wanted) == normalise_mime(mime))
}

/// The compositor-side half of the bridge, stored in `Compositor`.
pub struct ClipboardBridge {
    /// Compositor -> host. The reading thread pushes text the nested client copied.
    pub to_host_tx: Sender<String>,
    /// The receiver handed to the host so it can set its own clipboard.
    pub to_host_rx: Receiver<String>,
    /// Host -> compositor. The host pushes text it observed on its own clipboard.
    pub from_host_tx: Sender<String>,
    /// Drained by `service` on the compositor thread.
    pub from_host_rx: Receiver<String>,
    /// The host clipboard text we currently offer to the client, served by
    /// `send_host_text` when the client pastes.
    pub host_text: Option<String>,
    /// LOOP GUARD, compositor side: the last text we pushed toward the host. Shared with
    /// the short-lived reader threads because they are the ones that push. If the host
    /// hands that same text straight back (its poll sees the value we just set), we must
    /// not turn around and replace the client's own selection with a copy of itself.
    pub last_pushed_to_host: Arc<Mutex<Option<String>>>,
    /// Set by `SelectionHandler::new_selection`, consumed by `service`.
    ///
    /// Smithay calls `new_selection` BEFORE it stores the new selection in the seat, so
    /// `request_data_device_client_selection` would still see the previous one. We
    /// therefore only record which mime type to ask for and do the actual read after the
    /// dispatch has finished.
    pub pending_client_mime: Option<String>,
}

impl ClipboardBridge {
    /// Create both channels and the empty bookkeeping.
    pub fn new() -> Self {
        let (to_host_tx, to_host_rx) = crossbeam_channel::unbounded::<String>();
        let (from_host_tx, from_host_rx) = crossbeam_channel::unbounded::<String>();

        Self {
            to_host_tx,
            to_host_rx,
            from_host_tx,
            from_host_rx,
            host_text: None,
            last_pushed_to_host: Arc::new(Mutex::new(None)),
            pending_client_mime: None,
        }
    }
}

impl Default for ClipboardBridge {
    fn default() -> Self {
        Self::new()
    }
}

/// Record that the client set a clipboard selection (called from `new_selection`).
///
/// Only the chosen mime type is remembered here; see `pending_client_mime` for why the
/// read itself is deferred to `service`.
pub fn note_client_selection(state: &mut Compositor, source: Option<&SelectionSource>) {
    let Some(source) = source else {
        state.clipboard.pending_client_mime = None;
        return;
    };

    let offered = source.mime_types();
    match preferred_text_mime(&offered) {
        Some(mime) => {
            debug!("clipboard bridge: client set a selection, will read mime {mime}");
            state.clipboard.pending_client_mime = Some(mime);
        }
        None => {
            debug!("clipboard bridge: client selection offers no text mime ({offered:?}), ignoring");
            state.clipboard.pending_client_mime = None;
        }
    }
}

/// Serve the stored host clipboard text to the client (called from `send_selection`).
///
/// The client is on the other end of `fd`; a big payload could fill the pipe buffer and
/// block, so the write runs on a short-lived thread. Dropping the `File` closes the fd,
/// which is what signals end-of-data to the client's reader.
pub fn send_host_text(state: &mut Compositor, mime_type: &str, fd: OwnedFd) {
    if !is_text_mime(mime_type) {
        warn!("clipboard bridge: client asked for non-text mime {mime_type}, refusing");
        return;
    }

    let Some(text) = state.clipboard.host_text.clone() else {
        warn!("clipboard bridge: client pasted but no host text is stored");
        return;
    };

    let spawned = std::thread::Builder::new()
        .name("nws-clip-write".to_string())
        .spawn(move || {
            let mut file = File::from(fd);
            if let Err(err) = file.write_all(text.as_bytes()) {
                warn!("clipboard bridge: writing the host text to the client failed: {err}");
            }
        });

    if let Err(err) = spawned {
        warn!("clipboard bridge: spawning the clipboard writer thread failed: {err}");
    }
}

/// Run one bridge step on the compositor thread. Called once per event-loop iteration.
pub fn service(state: &mut Compositor) {
    apply_host_text(state);
    read_client_selection(state);
}

/// HOST -> NESTED. Take the newest text the host observed and offer it to the client.
fn apply_host_text(state: &mut Compositor) {
    let receiver = state.clipboard.from_host_rx.clone();

    // Only the newest value matters; a burst of poll results collapses to one offer.
    let mut newest: Option<String> = None;
    while let Ok(text) = receiver.try_recv() {
        newest = Some(text);
    }

    let Some(text) = newest else {
        return;
    };

    // LOOP GUARD: this is the text we ourselves pushed to the host a moment ago, so the
    // client already owns an identical selection. Re-offering it would steal the
    // selection from the client for no reason.
    let echoed = state
        .clipboard
        .last_pushed_to_host
        .lock()
        .map(|last| last.as_deref() == Some(text.as_str()))
        .unwrap_or(false);
    if echoed {
        return;
    }

    // Nothing changed since the last offer.
    if state.clipboard.host_text.as_deref() == Some(text.as_str()) {
        return;
    }

    debug!("clipboard bridge: offering {} host bytes to the client", text.len());
    state.clipboard.host_text = Some(text);

    let display_handle = state.display_handle.clone();
    let seat = state.seat.clone();
    set_data_device_selection::<Compositor>(&display_handle, &seat, text_mime_types(), ());
}

/// NESTED -> HOST. Ask the client to write its selection into a pipe and read it off the
/// event loop.
fn read_client_selection(state: &mut Compositor) {
    let Some(mime) = state.clipboard.pending_client_mime.take() else {
        return;
    };

    let (reader, writer) = match std::io::pipe() {
        Ok(pair) => pair,
        Err(err) => {
            warn!("clipboard bridge: creating the selection pipe failed: {err}");
            return;
        }
    };

    let seat = state.seat.clone();

    // Hands the write end to the client and closes our copy, so the reader below sees EOF
    // as soon as the client is done writing.
    if let Err(err) =
        request_data_device_client_selection::<Compositor>(&seat, mime.clone(), OwnedFd::from(writer))
    {
        warn!("clipboard bridge: requesting the client selection ({mime}) failed: {err}");
        return;
    }

    let sender = state.clipboard.to_host_tx.clone();
    let last_pushed = Arc::clone(&state.clipboard.last_pushed_to_host);

    // The client can only write once we return to the event loop and the display is
    // flushed, so this read MUST NOT happen on the compositor thread.
    let spawned = std::thread::Builder::new()
        .name("nws-clip-read".to_string())
        .spawn(move || {
            let mut reader = reader;
            let mut bytes = Vec::new();
            if let Err(err) = reader.read_to_end(&mut bytes) {
                warn!("clipboard bridge: reading the client selection failed: {err}");
                return;
            }

            match String::from_utf8(bytes) {
                Ok(text) => {
                    debug!("clipboard bridge: client selection is {} bytes", text.len());
                    if let Ok(mut last) = last_pushed.lock() {
                        *last = Some(text.clone());
                    }
                    let _ = sender.send(text);
                }
                Err(err) => {
                    warn!("clipboard bridge: client selection was not valid UTF-8: {err}");
                }
            }
        });

    if let Err(err) = spawned {
        warn!("clipboard bridge: spawning the clipboard reader thread failed: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::{is_text_mime, preferred_text_mime, text_mime_types};

    #[test]
    fn utf8_text_wins_over_plain_text() {
        let offered = vec![
            "text/html".to_string(),
            "text/plain".to_string(),
            "text/plain;charset=utf-8".to_string(),
        ];
        assert_eq!(
            preferred_text_mime(&offered),
            Some("text/plain;charset=utf-8".to_string())
        );
    }

    #[test]
    fn mime_matching_ignores_case_and_spaces() {
        let offered = vec!["TEXT/PLAIN; charset=UTF-8".to_string()];
        assert_eq!(
            preferred_text_mime(&offered),
            Some("TEXT/PLAIN; charset=UTF-8".to_string())
        );
        assert!(is_text_mime("text/plain ; CHARSET=utf-8"));
    }

    #[test]
    fn non_text_selections_are_rejected() {
        let offered = vec!["image/png".to_string(), "text/uri-list".to_string()];
        assert_eq!(preferred_text_mime(&offered), None);
        assert!(!is_text_mime("image/png"));
    }

    #[test]
    fn advertised_mimes_are_the_three_text_types() {
        assert_eq!(
            text_mime_types(),
            vec![
                "text/plain;charset=utf-8".to_string(),
                "text/plain".to_string(),
                "UTF8_STRING".to_string(),
            ]
        );
    }
}
