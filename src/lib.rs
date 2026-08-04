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
