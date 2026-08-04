//! The half that knows a window exists.
//!
//! Widget trees are built in Rust — no `.ui` XML, no Blueprint, no GResource.
//! The structure of a pane is then readable in the same file as the behaviour
//! that drives it, which for an app this size is worth more than a designer
//! could give back.

pub mod client;
pub mod embedder;

mod application;
mod approval;
mod composer;
mod conversation;
pub mod dialogs;
pub mod file_tree;
pub mod mail;
mod markdown;
pub mod preferences;
pub mod project_view;
pub mod runner;
pub mod sidebar;
mod staging;
pub mod tool_detail;
pub mod turn;
mod turn_view;
mod window;
pub mod workflow_bar;

pub use application::Application;
pub use composer::Composer;
pub use conversation::Conversation;
pub use file_tree::FileTree;
/// Where a point in an answer links to. Exposed so a test can point at every
/// pixel of one: hit-testing a buffer full of hidden syntax is where GTK's own
/// pixel-to-character conversion aborts the process.
pub use markdown::target_at as link_at;
pub use project_view::ProjectView;
pub use sidebar::Sidebar;
pub use staging::Staging;
pub use turn::{Chip, ToolChip, Turn};
pub use turn_view::TurnView;
pub use window::Window;
pub use workflow_bar::WorkflowBar;

/// The application stylesheet, compiled in.
pub const STYLE: &str = include_str!("style.css");

/// Load the stylesheet at application priority, above the theme and below the
/// user's own overrides.
/// A folder as somebody would say where it is: `~/Projects/familiar`.
///
/// Shared because two places show a path to a person — the Files row in the
/// sidebar and the sentence that says a deleted project's folder is not
/// touched — and a home directory spelled out in full is noise in both.
pub fn home_relative(path: &std::path::Path) -> String {
    let shown = path.display().to_string();
    match gtk::glib::home_dir().to_str() {
        Some(home) if shown.starts_with(home) => format!("~{}", &shown[home.len()..]),
        _ => shown,
    }
}

pub fn load_stylesheet(display: &gtk::gdk::Display) {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(STYLE);
    gtk::style_context_add_provider_for_display(
        display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}
