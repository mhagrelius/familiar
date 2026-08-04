//! The reverse fold: a turn's state becomes a turn's widget.
//!
//! `TurnStream` folds frames into [`TurnState`]; this folds `TurnState` into a
//! [`Turn`] widget. The live stream and a thread reopened from disk both come
//! through here, which is what stops a reopened conversation from looking
//! different from the one you just had.
//!
//! It owns the render throttle. A local model on a 5090 emits tokens faster
//! than a frame lasts, and a re-layout per token is wasted work; text
//! accumulates and is flushed on a timer, with the tail always flushed on
//! settle so nothing can be left in the buffer.
//!
//! It also owns the thinking-pane settle policy: "Thinking…" while reasoning
//! streams, "Thought for 4s" once it stops. The *duration* is not measured
//! here — `TurnStream` holds the only clock, and the number is persisted with
//! the turn so a reopened thread says the same thing.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk::glib;

use crate::model::thread::StoredTurn;
use crate::model::turn::{Event, ToolCall, TurnState};
use crate::ui::turn::Chip;
use crate::ui::Turn;

/// Long enough to coalesce a burst, short enough to read as live typing.
const FLUSH: Duration = Duration::from_millis(50);

pub struct TurnView {
    widget: Turn,
    answer: RefCell<String>,
    thinking: RefCell<String>,
    /// Whether the preference says to draw the disclosure at all.
    show_thinking: bool,
    /// Text has arrived that is not on screen yet.
    dirty: Cell<bool>,
    flush: RefCell<Option<glib::SourceId>>,
}

impl TurnView {
    /// A view for a turn that is about to be asked.
    pub fn new(question: &str, show_thinking: bool) -> Rc<Self> {
        let widget = Turn::new();
        widget.set_question(question);
        widget.set_pending(true);
        Rc::new(Self {
            widget,
            answer: RefCell::new(String::new()),
            thinking: RefCell::new(String::new()),
            show_thinking,
            dirty: Cell::new(false),
            flush: RefCell::new(None),
        })
    }

    /// A view for a turn that already happened.
    ///
    /// `images` are already-decoded textures: loading them is the
    /// application's job, because it is the only thing that knows which
    /// project's directory they live in.
    pub fn replayed_with(
        turn: &StoredTurn,
        show_thinking: bool,
        images: &[gtk::gdk::Texture],
    ) -> Rc<Self> {
        let view = Self::replayed(turn, show_thinking);
        view.widget.set_images(images);
        view
    }

    /// A view for a turn that already happened.
    pub fn replayed(turn: &StoredTurn, show_thinking: bool) -> Rc<Self> {
        let view = Self::new(&turn.user, show_thinking);
        view.widget.set_pending(false);
        view.answer.replace(turn.answer.clone());
        view.thinking.replace(turn.thinking.clone());
        view.widget.set_answer(&turn.answer);
        view.widget.set_thinking(&turn.thinking, show_thinking);
        view.widget
            .set_thinking_summary(&thought_for(turn.metrics.and_then(|m| m.thinking_ms)));
        view.widget
            .set_metrics(&turn.metrics.map(|m| m.one_line()).unwrap_or_default());
        // A reopened thread used to lose its chips entirely, so what the
        // assistant actually did was visible for as long as the window stayed
        // open and no longer. Everything needed is on disk — `StoredToolCall`
        // keeps the arguments and the outcome — and the same fold draws them.
        view.widget.set_tool_calls(
            &turn
                .tool_calls
                .iter()
                .map(|stored| Chip::of(&ToolCall::from(stored)))
                .collect::<Vec<_>>(),
        );
        if turn.was_truncated() {
            view.widget
                .set_failure(Some("The answer was cut off by the token limit."));
        }
        view
    }

    /// Show the images the question was asked with.
    pub fn set_images(&self, images: &[gtk::gdk::Texture]) {
        self.widget.set_images(images);
    }

    pub fn widget(&self) -> &Turn {
        &self.widget
    }

    /// One event off the stream.
    pub fn apply(self: &Rc<Self>, event: &Event) {
        match event {
            Event::Answer(fragment) => {
                self.answer.borrow_mut().push_str(fragment);
                self.schedule_flush();
            }
            Event::Thinking(fragment) => {
                self.thinking.borrow_mut().push_str(fragment);
                self.schedule_flush();
            }
            Event::Failed(error) => self.widget.set_failure(Some(&error.to_string())),
            Event::ToolCall(_) | Event::Measured | Event::Finished(_) => {}
        }
    }

    /// The turn is over. Flushes whatever is buffered and renders the settled
    /// answer, which is not always the accumulated one — a leaked tool call is
    /// stripped at this point.
    pub fn settle(&self, state: &TurnState) {
        self.cancel_flush();
        self.answer.replace(state.answer.clone());
        self.thinking.replace(state.thinking.clone());

        self.widget.set_pending(false);
        self.widget
            .set_thinking(&state.thinking, self.show_thinking);
        self.widget.set_thinking_summary(&thought_for(
            state.thinking_elapsed.map(|d| d.as_millis() as u64),
        ));
        self.widget.set_answer(&state.answer);
        self.widget.set_metrics(&state.metrics().one_line());
    }

    pub fn set_failure(&self, text: Option<&str>) {
        self.widget.set_pending(false);
        self.widget.set_failure(text);
    }

    fn schedule_flush(self: &Rc<Self>) {
        self.dirty.set(true);
        if self.flush.borrow().is_some() {
            return;
        }
        // The timeout holds a strong reference, and drops it the moment it
        // finds nothing left to draw — so the cycle lasts one tick past the
        // last token rather than for the life of the view.
        let view = self.clone();
        let source = glib::timeout_add_local(FLUSH, move || {
            if !view.dirty.replace(false) {
                view.flush.replace(None);
                return glib::ControlFlow::Break;
            }
            view.draw();
            glib::ControlFlow::Continue
        });
        self.flush.replace(Some(source));
    }

    /// Mid-stream drawing. The answer is rendered as it arrives, half-written
    /// syntax and all — the alternative is prose that reflows every time a
    /// `**` closes, which is worse to read than a stray asterisk.
    fn draw(&self) {
        self.widget.set_pending(false);
        let thinking = self.thinking.borrow();
        if !thinking.is_empty() {
            self.widget.set_thinking(&thinking, self.show_thinking);
            self.widget.set_thinking_summary("Thinking…");
        }
        drop(thinking);
        self.widget.set_answer(&self.answer.borrow());
    }

    fn cancel_flush(&self) {
        if let Some(source) = self.flush.borrow_mut().take() {
            source.remove();
        }
        self.dirty.set(false);
    }
}

impl Drop for TurnView {
    fn drop(&mut self) {
        self.cancel_flush();
    }
}

/// What the disclosure says once the thinking has stopped.
///
/// Seconds, never milliseconds: the number is there to tell you whether the
/// model deliberated for a moment or for a minute, and three decimal places
/// answer a question nobody asked.
fn thought_for(milliseconds: Option<u64>) -> String {
    let Some(milliseconds) = milliseconds else {
        return "Thinking…".to_string();
    };
    let seconds = milliseconds as f64 / 1000.0;
    if seconds < 60.0 {
        format!("Thought for {seconds:.0}s")
    } else {
        let minutes = (seconds / 60.0).floor();
        let rest = seconds - minutes * 60.0;
        format!("Thought for {minutes:.0}m {rest:.0}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_disclosure_reports_seconds_and_minutes() {
        assert_eq!(thought_for(Some(4_300)), "Thought for 4s");
        assert_eq!(thought_for(Some(400)), "Thought for 0s");
        assert_eq!(thought_for(Some(95_000)), "Thought for 1m 35s");
        assert_eq!(thought_for(None), "Thinking…");
    }
}
