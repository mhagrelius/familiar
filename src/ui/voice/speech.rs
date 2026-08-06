//! Where the speech models are, and the worker that runs them.
//!
//! The worker is [`earshot::Speech`] — two models, one ONNX Runtime, one thread,
//! answering through `glib::idle_add_once`. What is here is the part the crate
//! deliberately does not decide: **which directory to look in**. Scribe owns its
//! models; this application reads Scribe's copy if it finds one, and that
//! difference is why the crate takes a resolver instead of a path.
//!
//! **The live text is feedback, not the question.** It is on screen so the
//! speaker can see they are being heard, and it is thrown away: what gets sent
//! is the accurate pass over the whole utterance. The two disagree often enough
//! at the edges of words that sending the streaming text would mean asking a
//! question nobody asked.

use gtk::glib;
use std::path::PathBuf;

pub use earshot::{Model, SpeechError, STREAM_CHUNK};

/// Where this application keeps model files.
fn own_dir() -> PathBuf {
    glib::user_data_dir().join("familiar").join("models")
}

/// Where Scribe keeps them.
///
/// Read, never written. Scribe is the sibling app that already downloads these
/// two models and is likely to be installed beside this one; asking somebody to
/// keep a second 700 MB copy of the same files because two applications of ours
/// disagree about a directory is not a thing to do. If Scribe is not installed
/// this is simply a path that does not exist.
fn scribe_dir() -> PathBuf {
    glib::user_data_dir().join("scribe").join("models")
}

/// The directory holding `model`, if either place has it.
pub fn model_dir(model: Model) -> Option<PathBuf> {
    [own_dir(), scribe_dir()]
        .into_iter()
        .map(|root| root.join(model.folder()))
        .find(|dir| earshot::is_complete(dir, model))
}

/// Whether anything can be transcribed at all.
pub fn is_installed() -> bool {
    model_dir(Model::Accurate).is_some() || model_dir(Model::Live).is_some()
}

/// What to tell somebody who has no model.
pub const MISSING: &str = "No speech model is installed. Familiar reads the two Parakeet models \
     from ~/.local/share/familiar/models (or Scribe's copy, if that is installed). \
     See the Voice section of the README for the two commands that fetch them.";

/// The worker, resolving models the way this application does.
///
/// `model_dir` is called per job rather than once, so a model fetched while the
/// application is running is picked up without a restart.
pub struct Speech(earshot::Speech);

impl Default for Speech {
    fn default() -> Self {
        Self::new()
    }
}

impl Speech {
    pub fn new() -> Self {
        Self(earshot::Speech::new("familiar-speech", model_dir))
    }

    /// Transcribe a finished utterance. `done` runs on the main loop.
    pub fn transcribe(
        &self,
        audio: Vec<f32>,
        done: impl FnOnce(Result<String, SpeechError>) + 'static,
    ) {
        self.0.transcribe(audio, done);
    }

    /// Feed one chunk to the live model. `done` runs on the main loop.
    pub fn feed(&self, audio: Vec<f32>, done: impl FnOnce(Result<String, SpeechError>) + 'static) {
        self.0.feed(audio, done);
    }

    /// Forget the streaming encoder's state before a new utterance.
    pub fn reset(&self) {
        self.0.reset();
    }
}

/// What to show for a failure, with this application's advice attached.
///
/// The crate's own sentence for a missing model is short, because it has no idea
/// where this application looks for one. [`MISSING`] does, and a message that
/// names the two directories and the script that fills them is the difference
/// between a dead end and something to do next.
pub fn trouble(error: &SpeechError) -> String {
    match error {
        SpeechError::NotInstalled(_) => MISSING.to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_models_are_looked_for_in_two_places() {
        // Scribe's copy is the whole reason this is not one path.
        assert_ne!(own_dir(), scribe_dir());
    }

    #[test]
    fn a_missing_model_is_reported_with_somewhere_to_go() {
        // The crate says "not installed"; only this side knows where from.
        let said = trouble(&SpeechError::NotInstalled(Model::Accurate));
        assert!(said.contains("models"), "{said}");
        assert!(said.len() > 40, "the bare crate sentence is not enough");
    }
}
