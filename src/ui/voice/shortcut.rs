//! The global shortcut.
//!
//! The obvious answer is the `GlobalShortcuts` portal, which hands out press
//! *and* release and would give push-to-talk for free. It does not work here,
//! and that was measured rather than assumed — first for Scribe, on this
//! desktop, against this portal. Since xdg-desktop-portal 1.21 the interface
//! refuses any caller without an application identity, and the mechanism a
//! non-Flatpak application uses to declare one,
//! `org.freedesktop.host.portal.Registry`, is not exported by the portal
//! running here: the bus name is owned by nothing. `CreateSession` comes back
//! `NotAllowed: An app id is required`. Launching under a systemd scope named
//! after the app does not stand in for it either.
//!
//! So the shortcut is a gnome-settings-daemon custom keybinding — the same
//! thing the Settings app's own Keyboard panel writes — running `familiar
//! --voice`. It needs no consent dialog, no portal and no privileges, and it
//! survives a reboot because it lives in dconf rather than in this process.
//!
//! What it cannot do is tell a press from a release, because gsd spawns a
//! command on activation and there is no second event. **So listening is a
//! toggle, and silence is what ends an utterance** — see
//! `model::voice::Endpointer`. Push-to-talk would mean reading `/dev/input`
//! directly, which means putting the user in the `input` group and handing a
//! keylogger to every process they run. Not worth one key.
//!
//! A sandbox supplies the app id the portal wants, so a Flatpak build of this
//! could use it. There is no Flatpak build — see `DESIGN.md` — and there would
//! be no point in one that could not spawn `pw-record` anyway.

use gio::prelude::*;

const MEDIA_KEYS: &str = "org.gnome.settings-daemon.plugins.media-keys";
const CUSTOM_KEYBINDING: &str = "org.gnome.settings-daemon.plugins.media-keys.custom-keybinding";
const PATH: &str = "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/familiar/";
const NAME: &str = "Talk to Familiar";

/// The default. `<Super><Alt>` is where GNOME leaves room for applications, and
/// this key is not one GNOME has spoken for — which [`conflict`] checks anyway,
/// because registering over one of GNOME's own fails silently and leaves both
/// actions firing.
pub const DEFAULT_ACCELERATOR: &str = "<Super><Alt>space";

/// Where GNOME keeps the shortcuts it has already taken.
const RESERVED_SCHEMAS: &[&str] = &[
    "org.gnome.desktop.wm.keybindings",
    "org.gnome.shell.keybindings",
    "org.gnome.mutter.keybindings",
    "org.gnome.mutter.wayland.keybindings",
    "org.gnome.settings-daemon.plugins.media-keys",
];

/// Why a shortcut could not be registered.
#[derive(Debug)]
pub enum ShortcutError {
    /// gnome-settings-daemon's schema is not installed, so this is not GNOME.
    Unsupported,
    /// The accelerator is not one GTK can parse.
    Unparsable(String),
}

impl std::fmt::Display for ShortcutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShortcutError::Unsupported => write!(
                f,
                "This desktop does not provide GNOME's custom keyboard shortcuts, so Familiar \
                 cannot register one. Bind a shortcut to “familiar --voice” in your desktop's \
                 own keyboard settings instead."
            ),
            ShortcutError::Unparsable(accel) => {
                write!(
                    f,
                    "“{accel}” is not a keyboard shortcut Familiar understands."
                )
            }
        }
    }
}

impl std::error::Error for ShortcutError {}

/// Whether this desktop has the settings we would be writing into.
pub fn is_supported() -> bool {
    let Some(source) = gio::SettingsSchemaSource::default() else {
        return false;
    };
    source.lookup(MEDIA_KEYS, true).is_some() && source.lookup(CUSTOM_KEYBINDING, true).is_some()
}

/// The command the shortcut runs.
///
/// An absolute path rather than a bare `familiar`: gnome-settings-daemon spawns
/// with its own environment, and a binary under `~/.local/bin` is not reliably
/// on the `PATH` it inherits. The running instance answers it over D-Bus —
/// the application handles its own command line — so this starts nothing new.
pub fn command() -> String {
    let binary = std::env::current_exe()
        .ok()
        .filter(|path| path.is_absolute())
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "familiar".to_string());
    format!("{binary} --voice")
}

/// Check an accelerator before it is written anywhere.
pub fn is_parsable(accel: &str) -> bool {
    let Some(parsed) = gtk::accelerator_parse(accel) else {
        return false;
    };
    // `accelerator_parse` does not fail outright on nonsense: it hands back the
    // void symbol, or for an empty string a keyval of zero. Neither has a key
    // name, which is the one check that catches both.
    parsed.0 != gtk::gdk::Key::VoidSymbol && parsed.0.name().is_some()
}

/// Two accelerators are the same key if they parse the same, however spelled.
///
/// Through the parser rather than a string comparison, because GNOME writes
/// `<Primary>` where the Keyboard panel shows Ctrl, and comparing strings would
/// miss every collision that matters.
fn same_key(a: &str, b: &str) -> bool {
    match (gtk::accelerator_parse(a), gtk::accelerator_parse(b)) {
        (Some(left), Some(right)) => {
            left.0 != gtk::gdk::Key::VoidSymbol
                && left.0.name().is_some()
                && left.0 == right.0
                && left.1 == right.1
        }
        _ => false,
    }
}

/// What GNOME already uses `accel` for, if anything.
pub fn conflict(accel: &str) -> Option<String> {
    let source = gio::SettingsSchemaSource::default()?;
    for schema_id in RESERVED_SCHEMAS {
        let Some(schema) = source.lookup(schema_id, true) else {
            continue;
        };
        let settings = gio::Settings::new(schema_id);
        for key in schema.list_keys() {
            // Only the string-array keys are accelerator lists, and this has to
            // be checked before reading: `Settings::strv` on a key of another
            // type aborts the process rather than returning an error.
            if schema.key(&key).value_type().as_str() != "as" {
                continue;
            }
            if settings
                .strv(&key)
                .iter()
                .any(|bound| same_key(bound.as_str(), accel))
            {
                return Some(key.to_string());
            }
        }
    }
    None
}

/// Install or update the binding, so pressing `accel` starts listening.
pub fn install(accel: &str) -> Result<(), ShortcutError> {
    if !is_supported() {
        return Err(ShortcutError::Unsupported);
    }
    if !is_parsable(accel) {
        return Err(ShortcutError::Unparsable(accel.to_string()));
    }

    let binding = gio::Settings::with_path(CUSTOM_KEYBINDING, PATH);
    binding.set_string("name", NAME).ok();
    binding.set_string("binding", accel).ok();
    binding.set_string("command", &command()).ok();

    let media_keys = gio::Settings::new(MEDIA_KEYS);
    let mut paths: Vec<String> = media_keys
        .strv("custom-keybindings")
        .iter()
        .map(|path| path.to_string())
        .collect();
    if !paths.iter().any(|path| path == PATH) {
        paths.push(PATH.to_string());
        let borrowed: Vec<&str> = paths.iter().map(String::as_str).collect();
        media_keys.set_strv("custom-keybindings", borrowed).ok();
    }
    gio::Settings::sync();
    Ok(())
}

/// Take the binding back out, leaving the user's other shortcuts alone.
pub fn remove() {
    if !is_supported() {
        return;
    }
    let media_keys = gio::Settings::new(MEDIA_KEYS);
    let paths: Vec<String> = media_keys
        .strv("custom-keybindings")
        .iter()
        .map(|path| path.to_string())
        .filter(|path| path != PATH)
        .collect();
    let borrowed: Vec<&str> = paths.iter().map(String::as_str).collect();
    media_keys.set_strv("custom-keybindings", borrowed).ok();

    let binding = gio::Settings::with_path(CUSTOM_KEYBINDING, PATH);
    for key in ["name", "binding", "command"] {
        binding.reset(key);
    }
    gio::Settings::sync();
}

/// What is registered right now, if anything.
pub fn installed() -> Option<String> {
    if !is_supported() {
        return None;
    }
    let media_keys = gio::Settings::new(MEDIA_KEYS);
    let registered = media_keys
        .strv("custom-keybindings")
        .iter()
        .any(|path| path.as_str() == PATH);
    if !registered {
        return None;
    }
    let binding = gio::Settings::with_path(CUSTOM_KEYBINDING, PATH);
    let accel = binding.string("binding").to_string();
    (!accel.is_empty()).then_some(accel)
}

/// The accelerator as a person would read it: "Super+Alt+Space".
pub fn human_label(accel: &str) -> String {
    let Some((key, modifiers)) = gtk::accelerator_parse(accel) else {
        return accel.to_string();
    };
    if key == gtk::gdk::Key::VoidSymbol || key.name().is_none() {
        return accel.to_string();
    }
    let label = gtk::accelerator_get_label(key, modifiers);
    if label.is_empty() {
        accel.to_string()
    } else {
        label.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Anything calling `accelerator_parse` needs a display, so it lives with
    // the widget tests. What can be checked without one is where the binding
    // lands and what it runs, which is what decides whether GNOME finds it.

    #[test]
    fn the_command_carries_the_voice_flag() {
        let command = command();
        assert!(command.ends_with(" --voice"), "got {command}");
    }

    #[test]
    fn the_binding_lives_under_a_path_of_our_own() {
        // Reusing GNOME's "custom0" would collide with whatever the user has
        // already bound there.
        assert!(PATH.ends_with("/familiar/"));
        assert!(PATH.starts_with("/org/gnome/settings-daemon/"));
    }

    #[test]
    fn the_default_is_not_one_of_gnomes_own_spellings() {
        // A sanity check on the constant, not on the running desktop:
        // `<Primary><Alt>d` is Show Desktop and cost Scribe an afternoon.
        assert!(!DEFAULT_ACCELERATOR.contains("<Primary>"));
    }
}
