//! The microphone, and the list of microphones.
//!
//! Capture itself is [`earshot`]: `pw-record` on a pipe read by the main loop,
//! samples handed over as `f32`, and the loudness curve every threshold in
//! `model::voice` is calibrated against. It lived here first, in a copy Scribe
//! also had; the two drifted, and a bug fixed in one stayed in the other.
//!
//! What stays is the part that is this application's: which capture devices
//! exist, so Preferences can offer them.

pub use earshot::{level, Recorder, StartError, SAMPLE_RATE};

/// A microphone somebody could pick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// What `pw-record --target` wants.
    pub name: String,
    /// What a person calls it.
    pub description: String,
}

/// Every capture device PipeWire knows about.
///
/// Asked for by `pw-dump`, which ships beside `pw-record`, rather than by
/// linking a PipeWire client: this is read once when a preferences page is
/// built, and a whole client library to fill one dropdown is not a trade worth
/// making. It blocks for a few tens of milliseconds, which is the one place in
/// this application that is acceptable — a dialog opening, not a turn running.
///
/// **Which microphone is not a detail.** A conference webcam applies noise
/// suppression in its own firmware and treats the assistant's voice coming out
/// of the speakers as noise to be removed — along with, it turns out, a good
/// deal of the person talking over it. Being able to point at a different one
/// is the difference between interrupting working and not. It also moves every
/// level in `model::voice::Endpointer` by an order of magnitude, which is what
/// invalidated the first set of them.
pub fn sources() -> Vec<Source> {
    let Ok(output) = std::process::Command::new("pw-dump").output() else {
        return Vec::new();
    };
    let Ok(objects) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for object in objects.as_array().unwrap_or(&Vec::new()) {
        let props = &object["info"]["props"];
        if props["media.class"].as_str() != Some("Audio/Source") {
            continue;
        }
        let Some(name) = props["node.name"].as_str() else {
            continue;
        };
        let description = props["node.description"]
            .as_str()
            .or_else(|| props["node.nick"].as_str())
            .unwrap_or(name);
        found.push(Source {
            name: name.to_string(),
            description: description.to_string(),
        });
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_speech_clears_the_endpointer_gate() {
        // The crate and this one agree on one scale or the gate is meaningless.
        // Speech at an RMS of 0.1 has to be heard as speech.
        let speech: Vec<f32> = (0..128)
            .map(|i| if i % 2 == 0 { 0.1 } else { -0.1 })
            .collect();
        let quiet: Vec<f32> = (0..128)
            .map(|i| if i % 2 == 0 { 0.002 } else { -0.002 })
            .collect();
        let endpointer = crate::model::voice::Endpointer::default();
        assert!(level(&speech) > endpointer.gate(), "{}", level(&speech));
        assert!(level(&quiet) < endpointer.gate(), "{}", level(&quiet));
    }

    #[test]
    fn the_two_halves_agree_on_how_long_a_block_is() {
        // `model::voice` counts in blocks and cannot import the crate — it is
        // the display-free half and `earshot` links GLib — so the constant
        // exists twice. Every clock in the endpointer is wrong if they diverge.
        assert_eq!(crate::model::voice::BLOCK_MS, earshot::BLOCK_MS);
    }
}
