//! Familiar: an assistant for GNOME, pointed at a local `llama-server`.
//!
//! Two halves. `model/` links no GTK and no HTTP client, and is exercised by
//! `cargo test` with no display and no server: it is the wire, the fold that
//! turns a stream into a turn, the threads and contexts on disk, and the
//! composition of the system prompt. `ui/` is the only half that knows a window
//! exists, and `ui::FamiliarApplication` will be the only thing that writes a
//! file.

pub mod model;
pub mod ui;

pub const APP_ID: &str = "us.hagreli.Familiar";

/// When the running binary was built, as far as it can tell.
///
/// Printed at the top of a voice trace. More than one evening has been spent
/// on a report from a binary that predated the fix being discussed, because
/// the shortcut hands its command to whichever instance is already running and
/// nothing in the output said which one that was.
pub fn built_at() -> String {
    std::env::current_exe()
        .and_then(|path| path.metadata())
        .and_then(|meta| meta.modified())
        .map(|when| {
            let stamp: chrono::DateTime<chrono::Local> = when.into();
            stamp.format("%Y-%m-%d %H:%M").to_string()
        })
        .unwrap_or_else(|_| "unknown".to_string())
}
