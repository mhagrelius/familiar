//! One turn on screen: what you asked, what it thought, and what it answered.
//!
//! A mechanical widget — setters and nothing else. Every decision about *when*
//! to set them belongs to [`super::turn_view::TurnView`], which is the fold
//! that both the live stream and a thread loaded from disk go through. If this
//! file starts deciding things, the two paths have started to diverge.
//!
//! It reads as a document rather than a messenger: your message is a card with
//! a dimmed attribution, the answer is prose at full measure with nothing drawn
//! around it, and the numbers are a caption underneath.
//!
//! Thinking is subordinate but not hidden. This model spends thousands of
//! characters reasoning about a one-line answer, so a disclosure that is
//! collapsed by default is the difference between a readable conversation and
//! a wall of deliberation.

use std::sync::OnceLock;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib::subclass::Signal;
use gtk::glib::{self, clone};

use crate::model::turn::{ToolCall, ToolOutcome};
use crate::ui::{markdown, tool_detail};

/// How much of a selection is quoted back before it is clipped.
///
/// Long enough for a paragraph, short enough that the follow-up question is not
/// most of the previous answer again. Clipping loses nothing the model needs:
/// the answer it came from is still in the thread.
const QUOTE: usize = 1000;

/// What a tool chip is currently saying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolChip {
    /// Arguments still arriving, or the call still running.
    Running,
    Done,
    Failed,
    /// Refused at the approval dialog.
    Denied,
}

/// One chip, and the call behind it.
///
/// The call is carried whole rather than reduced to a label because the chip
/// is a button now: what it opens is the arguments and the result, and neither
/// survives being flattened into a tuple on the way here.
#[derive(Debug, Clone)]
pub struct Chip {
    pub call: ToolCall,
    /// What the pill shows beside the tool's name. Usually the call's primary
    /// argument, but a slow tool replaces it with whatever it is reporting —
    /// for a four-minute transcript that is the difference between "working"
    /// and "frozen".
    pub argument: Option<String>,
    pub state: ToolChip,
}

impl Chip {
    /// A chip for a call, taking its state from the outcome it carries.
    pub fn of(call: &ToolCall) -> Self {
        Self {
            call: call.clone(),
            argument: call.primary_argument(),
            state: match call.outcome.as_ref() {
                Some(ToolOutcome::Ok(_)) => ToolChip::Done,
                Some(ToolOutcome::Failed(_)) => ToolChip::Failed,
                Some(ToolOutcome::Denied) => ToolChip::Denied,
                None => ToolChip::Running,
            },
        }
    }

    /// Say what it is doing instead of what it was asked.
    pub fn saying(mut self, progress: Option<String>) -> Self {
        if let Some(progress) = progress {
            self.argument = Some(progress);
        }
        self
    }
}

mod imp {
    use super::*;

    pub struct Turn {
        pub layout: gtk::Box,
        pub attribution: gtk::Label,
        pub images: adw::WrapBox,
        pub question: gtk::Label,
        pub thinking: gtk::Expander,
        pub thinking_label: gtk::Label,
        pub thinking_text: gtk::Label,
        pub tools: adw::WrapBox,
        pub answer: gtk::TextView,
        pub spinner: adw::Spinner,
        pub metrics: gtk::Label,
        pub failure: gtk::Label,
    }

    impl Default for Turn {
        fn default() -> Self {
            Self {
                layout: gtk::Box::new(gtk::Orientation::Vertical, 6),
                attribution: gtk::Label::new(Some("You")),
                images: adw::WrapBox::new(),
                question: gtk::Label::new(None),
                thinking: gtk::Expander::new(None),
                thinking_label: gtk::Label::new(Some("Thinking…")),
                thinking_text: gtk::Label::new(None),
                tools: adw::WrapBox::new(),
                answer: markdown::view(),
                spinner: adw::Spinner::new(),
                metrics: gtk::Label::new(None),
                failure: gtk::Label::new(None),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Turn {
        const NAME: &'static str = "FamiliarTurn";
        type Type = super::Turn;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_layout_manager_type::<gtk::BinLayout>();
        }
    }

    impl ObjectImpl for Turn {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().build();
        }

        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }

        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                // A whole question, not the selection it was built from: what
                // "Explain This" sends is this widget's business, and every
                // layer above it only has to carry the words along.
                vec![Signal::builder("explain")
                    .param_types([String::static_type()])
                    .build()]
            })
        }
    }

    impl WidgetImpl for Turn {}
}

glib::wrapper! {
    pub struct Turn(ObjectSubclass<imp::Turn>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for Turn {
    fn default() -> Self {
        Self::new()
    }
}

impl Turn {
    pub fn new() -> Self {
        glib::Object::new()
    }

    fn build(&self) {
        let imp = self.imp();

        imp.question.set_xalign(0.0);
        imp.question.set_wrap(true);
        imp.question.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        imp.question.set_selectable(true);

        // Without this the card reads as a text entry rather than as
        // something already said.
        imp.attribution.set_xalign(0.0);
        imp.attribution.add_css_class("caption-heading");
        imp.attribution.add_css_class("dimmed");

        imp.images.set_child_spacing(6);
        imp.images.set_line_spacing(6);
        imp.images.set_visible(false);

        let asked = gtk::Box::new(gtk::Orientation::Vertical, 2);
        asked.add_css_class("card");
        asked.add_css_class("turn-question");
        asked.append(&imp.attribution);
        asked.append(&imp.images);
        asked.append(&imp.question);

        // The disclosure's own label, so it can say "Thought for 4s" once the
        // turn settles and "Thinking…" before that.
        imp.thinking_label.add_css_class("caption");
        imp.thinking_label.add_css_class("dimmed");
        imp.thinking.set_label_widget(Some(&imp.thinking_label));
        imp.thinking.set_expanded(false);
        imp.thinking.set_visible(false);

        imp.thinking_text.set_xalign(0.0);
        imp.thinking_text.set_wrap(true);
        imp.thinking_text
            .set_wrap_mode(gtk::pango::WrapMode::WordChar);
        imp.thinking_text.set_selectable(true);
        imp.thinking_text.add_css_class("caption");
        imp.thinking_text.add_css_class("dimmed");
        imp.thinking_text.add_css_class("turn-thinking");
        imp.thinking.set_child(Some(&imp.thinking_text));

        // Tool calls, as chips carrying the tool, its argument and its result —
        // so "done" cannot mask a no-op or an error.
        imp.tools.set_child_spacing(4);
        imp.tools.set_line_spacing(4);
        imp.tools.set_visible(false);

        imp.answer.set_visible(false);

        // Shown until the first token arrives: a turn with nothing in it yet
        // should look like it is thinking, not like it failed.
        imp.spinner.set_halign(gtk::Align::Start);
        imp.spinner.set_visible(false);

        imp.metrics.set_xalign(0.0);
        imp.metrics.add_css_class("caption");
        imp.metrics.add_css_class("dimmed");
        imp.metrics.add_css_class("numeric");
        imp.metrics.set_visible(false);

        imp.failure.set_xalign(0.0);
        imp.failure.set_wrap(true);
        imp.failure.add_css_class("error");
        imp.failure.add_css_class("caption");
        imp.failure.set_visible(false);

        imp.layout.append(&asked);
        imp.layout.append(&imp.thinking);
        imp.layout.append(&imp.tools);
        imp.layout.append(&imp.spinner);
        imp.layout.append(&imp.answer);
        imp.layout.append(&imp.failure);
        imp.layout.append(&imp.metrics);
        imp.layout.set_parent(self);

        self.offer_to_explain();
    }

    /// Right-clicking a phrase in the answer offers to go further into it.
    ///
    /// `extra-menu` rather than a `GtkGestureClick` of our own: the view
    /// already puts Copy and Select All on the right button, and an item
    /// appended to that menu arrives where a reader is looking instead of
    /// replacing what they expect to find there.
    ///
    /// The action group is the turn's, so what it explains is *this* answer's
    /// selection and no focus is involved — the popover has taken the keyboard
    /// by the time the item is clicked.
    fn offer_to_explain(&self) {
        let imp = self.imp();

        let menu = gio::Menu::new();
        menu.append(Some("Explain This"), Some("turn.explain"));
        imp.answer.set_extra_menu(Some(&menu));

        let explain = gio::SimpleAction::new("explain", None);
        // Nothing selected is nothing to explain. The item stays in the menu
        // and greys out: one that quietly does nothing reads as a fault.
        explain.set_enabled(false);
        explain.connect_activate(clone!(
            #[weak(rename_to = turn)]
            self,
            move |_, _| {
                turn.explain();
            }
        ));

        let actions = gio::SimpleActionGroup::new();
        actions.add_action(&explain);
        self.insert_action_group("turn", Some(&actions));

        imp.answer
            .buffer()
            .connect_has_selection_notify(move |buffer| {
                explain.set_enabled(buffer.has_selection());
            });
    }

    /// Ask about what is selected in the answer, reporting whether there was
    /// anything selected to ask about.
    pub fn explain(&self) -> bool {
        let Some(selection) = self.selected_answer() else {
            return false;
        };
        self.emit_by_name::<()>("explain", &[&explaining(&selection)]);
        true
    }

    /// The prose selected in the answer, or `None` when none is.
    ///
    /// What is on screen, not what is in the buffer: the Markdown syntax is
    /// still there under an invisible tag, and asking for the hidden characters
    /// would quote `**this**` back with the asterisks the view took off.
    pub fn selected_answer(&self) -> Option<String> {
        let buffer = self.imp().answer.buffer();
        let (start, end) = buffer.selection_bounds()?;
        // A table is a widget anchored in the buffer, and all that stands for
        // it in the text is one object-replacement character.
        let selected = buffer
            .text(&start, &end, false)
            .replace('\u{FFFC}', "")
            .trim()
            .to_string();
        (!selected.is_empty()).then_some(selected)
    }

    pub fn set_question(&self, text: &str) {
        self.imp().question.set_text(text);
    }

    /// The images that were asked about, shown with the question they came
    /// with — a question about a picture is unreadable without it.
    pub fn set_images(&self, images: &[gtk::gdk::Texture]) {
        let row = &self.imp().images;
        while let Some(child) = row.first_child() {
            row.remove(&child);
        }
        for texture in images {
            let picture = gtk::Picture::for_paintable(texture);
            picture.set_content_fit(gtk::ContentFit::ScaleDown);
            picture.set_can_shrink(true);
            // Big enough to see, small enough that three of them do not push
            // the answer off the screen.
            picture.set_size_request(-1, 180);
            picture.add_css_class("turn-image");
            row.append(&picture);
        }
        row.set_visible(!images.is_empty());
    }

    /// The answer, rendered as Markdown.
    pub fn set_answer(&self, text: &str) {
        let answer = &self.imp().answer;
        markdown::render(answer, text);
        answer.set_visible(!text.is_empty());
    }

    /// The answer as the model wrote it. The syntax characters are hidden
    /// rather than removed, so they are still in the buffer and every offset
    /// the scanner reported still lines up — which is why this asks for them.
    pub fn answer_text(&self) -> String {
        let buffer = self.imp().answer.buffer();
        buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), true)
            .to_string()
    }

    /// The reasoning. Hidden entirely when there is none, or when the
    /// preference is off.
    pub fn set_thinking(&self, text: &str, show: bool) {
        let imp = self.imp();
        imp.thinking_text.set_text(text);
        imp.thinking.set_visible(show && !text.is_empty());
    }

    /// The disclosure's label: "Thinking…" while it streams, "Thought for 4s"
    /// once it is over.
    pub fn set_thinking_summary(&self, text: &str) {
        self.imp().thinking_label.set_text(text);
    }

    pub fn thinking_summary(&self) -> String {
        self.imp().thinking_label.text().to_string()
    }

    /// The reasoning currently held. A collapsed `GtkExpander` does not keep
    /// its child in the widget tree, so this is the only way to read it back.
    pub fn thinking_text(&self) -> String {
        self.imp().thinking_text.text().to_string()
    }

    /// Redraw the tool chips.
    ///
    /// Rebuilt rather than diffed: a turn has a handful of calls at most, and
    /// a chip showing the previous call's result would be the worst kind of
    /// wrong.
    pub fn set_tool_calls(&self, calls: &[Chip]) {
        let tools = &self.imp().tools;
        while let Some(child) = tools.first_child() {
            tools.remove(&child);
        }
        for entry in calls {
            let pill = chip(&entry.call.name, entry.argument.as_deref(), entry.state);
            tools.append(&tool_detail::clickable(&pill, &entry.call));
        }
        tools.set_visible(!calls.is_empty());
    }

    /// Waiting on the first token.
    pub fn set_pending(&self, pending: bool) {
        self.imp().spinner.set_visible(pending);
    }

    /// The numbers under a finished turn. An empty line is drawn as no line
    /// rather than as a line of zeroes.
    pub fn set_metrics(&self, text: &str) {
        let metrics = &self.imp().metrics;
        metrics.set_text(text);
        metrics.set_visible(!text.is_empty());
    }

    /// A refusal, a dead connection, or a frame that made no sense. Shown under
    /// the answer rather than replacing it: whatever arrived before the failure
    /// is still worth reading.
    pub fn set_failure(&self, text: Option<&str>) {
        let failure = &self.imp().failure;
        match text {
            Some(text) => {
                failure.set_text(text);
                failure.set_visible(true);
            }
            None => failure.set_visible(false),
        }
    }
}

/// What "Explain This" sends: the selection as a blockquote, then the ask.
///
/// Quoted rather than pointed at. The thread is folded as it grows, so the
/// answer a phrase came from may not be in the next request at all — carrying
/// the words is the only way the question still means something afterwards.
fn explaining(selection: &str) -> String {
    let quoted = clip(selection)
        .lines()
        .map(|line| match line.trim_end() {
            "" => ">".to_string(),
            line => format!("> {line}"),
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("{quoted}\n\nExplain this part of your answer in more detail.")
}

fn clip(selection: &str) -> String {
    if selection.chars().count() <= QUOTE {
        return selection.to_string();
    }
    let kept: String = selection.chars().take(QUOTE).collect();
    format!("{}…", kept.trim_end())
}

/// One chip: an icon for how it went, the tool, and what it was called with.
fn chip(name: &str, argument: Option<&str>, state: ToolChip) -> gtk::Widget {
    let (icon, css, tooltip) = match state {
        ToolChip::Running => ("content-loading-symbolic", "tool-running", "Running"),
        ToolChip::Done => ("object-select-symbolic", "tool-done", "Done"),
        ToolChip::Failed => ("dialog-warning-symbolic", "tool-failed", "Failed"),
        ToolChip::Denied => ("action-unavailable-symbolic", "tool-denied", "Not run"),
    };

    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(12);

    let label = gtk::Label::new(Some(&match argument {
        Some(argument) if !argument.is_empty() => format!("{name} · {argument}"),
        _ => name.to_string(),
    }));
    label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    // Narrow enough that a turn's calls pack two or three to a line rather than
    // one each: five searches down the side of an answer is a wall, and the
    // argument is a reminder of what was asked, not the record — the chip opens
    // onto that.
    label.set_max_width_chars(32);
    label.add_css_class("caption");

    let chip = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    chip.add_css_class("tool-chip");
    chip.add_css_class(css);
    chip.set_tooltip_text(Some(tooltip));
    chip.append(&image);
    chip.append(&label);
    chip.upcast()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_selection_comes_back_as_a_quote_and_an_ask() {
        let asked = explaining("the KV cache is reused for the longest stable prefix");
        assert_eq!(
            asked,
            "> the KV cache is reused for the longest stable prefix\n\n\
             Explain this part of your answer in more detail."
        );
    }

    /// Every line, or the second paragraph reads as the user's own words.
    #[test]
    fn a_paragraph_is_quoted_line_by_line() {
        let asked = explaining("first line\n\nsecond line");
        assert!(
            asked.starts_with("> first line\n>\n> second line\n\n"),
            "{asked}"
        );
    }

    #[test]
    fn a_long_selection_is_clipped_rather_than_sent_whole() {
        let long = "word ".repeat(400);
        let asked = explaining(&long);
        let quote = asked.lines().next().expect("a quoted line");
        // The budget, plus the "> " marker and the ellipsis at most.
        assert!(quote.chars().count() <= QUOTE + 3, "{}", quote.len());
        assert!(quote.ends_with('…'), "{quote}");
    }

    #[test]
    fn a_selection_that_fits_is_not_touched() {
        assert_eq!(clip("short"), "short");
    }
}
