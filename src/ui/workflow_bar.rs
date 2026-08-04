//! Where a workflow is up to, over the conversation.
//!
//! # Why this is pinned rather than drawn in the conversation
//!
//! The obvious place for a checklist is inline, next to the thinking disclosure
//! and the tool chips. It does not work, and the reason constrains the model
//! side too: **a workflow spans turns and chips do not**. A chip belongs to the
//! turn that made it, which is why `Turn::set_tool_calls` rebuilds a flat row
//! per turn. Draw the checklist inline and it renders at the turn that created
//! it and then scrolls away while steps 3, 4 and 5 happen off screen — or every
//! turn redraws it and the conversation becomes five copies of one list. Worse,
//! if the calls grouped under their step, turn 4's work would appear in a card
//! sitting up at turn 2: you ask a question and nothing happens where you are
//! looking.
//!
//! So the chips stay exactly where they are, each turn showing what *it* did,
//! and this shows where the whole job is. Two different questions, and it is
//! fine for both to be on screen.
//!
//! `AdwBottomSheet` is the pattern for that: a `bottom-bar` strip that is always
//! visible while a workflow is open, and a `sheet` that pulls up over the
//! conversation with the steps in it. Not modal — you have to be able to read
//! the plan and type a correction at the same time, which is the whole point of
//! being able to steer.
//!
//! It is deliberately *not* the window's bottom toolbar, where the model name
//! and the context gauge live. That strip is a passive caption; this one has
//! Start, Edit and Stop in it, and putting buttons there would make the caption
//! look clickable. The other half is scope: that bar is per-window state, and a
//! workflow belongs to one chat and should leave with it.
//!
//! Every state marker here is the tool chip's: `object-select-symbolic` for
//! done, `dialog-warning-symbolic` for stuck, `action-unavailable-symbolic` for
//! skipped. A step reads like the pills the user already knows because it is the
//! same vocabulary, which costs nothing and is one less thing to learn.

use std::sync::OnceLock;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib::subclass::Signal;
use gtk::glib::{self, clone};

use crate::model::workflow::{State, Workflow};

mod imp {
    use super::*;
    use std::cell::RefCell;

    pub struct WorkflowBar {
        pub sheet: adw::BottomSheet,
        /// The collapsed strip: where it is up to, and what you can do about it.
        pub summary: gtk::Label,
        pub start: gtk::Button,
        pub edit: gtk::Button,
        pub stop: gtk::Button,
        /// The goal, over the steps.
        pub goal: gtk::Label,
        pub progress: gtk::Label,
        /// The steps, rebuilt whenever the workflow changes.
        ///
        /// A `GtkListBox` with `.boxed-list` rather than an
        /// `AdwPreferencesGroup`: the rows are replaced on every change and only
        /// the list box has `remove_all`. It looks the same.
        pub steps: gtk::ListBox,
        pub held: RefCell<Option<Workflow>>,
    }

    impl Default for WorkflowBar {
        fn default() -> Self {
            Self {
                sheet: adw::BottomSheet::new(),
                summary: gtk::Label::new(None),
                start: gtk::Button::with_label("Start"),
                edit: gtk::Button::from_icon_name("document-edit-symbolic"),
                stop: gtk::Button::from_icon_name("process-stop-symbolic"),
                goal: gtk::Label::new(None),
                progress: gtk::Label::new(None),
                steps: gtk::ListBox::new(),
                held: RefCell::new(None),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for WorkflowBar {
        const NAME: &'static str = "FamiliarWorkflowBar";
        type Type = super::WorkflowBar;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_layout_manager_type::<gtk::BinLayout>();
        }
    }

    impl ObjectImpl for WorkflowBar {
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
                vec![
                    // The user said go. Distinct from the model deciding to
                    // begin, because it is the one the eval family is about.
                    Signal::builder("start").build(),
                    Signal::builder("edit").build(),
                    Signal::builder("stop").build(),
                ]
            })
        }
    }

    impl WidgetImpl for WorkflowBar {}
}

glib::wrapper! {
    pub struct WorkflowBar(ObjectSubclass<imp::WorkflowBar>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for WorkflowBar {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkflowBar {
    pub fn new() -> Self {
        glib::Object::new()
    }

    /// What the sheet is wrapped around: the conversation goes in here, so the
    /// strip lands directly above the composer rather than at the very bottom of
    /// the window. That is where somebody about to type a correction is already
    /// looking.
    pub fn set_content(&self, content: &impl IsA<gtk::Widget>) {
        self.imp().sheet.set_content(Some(content));
    }

    fn build(&self) {
        let imp = self.imp();

        imp.summary.set_xalign(0.0);
        imp.summary.set_hexpand(true);
        imp.summary.set_ellipsize(gtk::pango::EllipsizeMode::End);
        imp.summary.add_css_class("caption-heading");

        imp.start.add_css_class("suggested-action");
        imp.start.add_css_class("pill");
        imp.start.set_tooltip_text(Some("Start This Workflow"));

        imp.edit.set_tooltip_text(Some("Edit Steps"));
        imp.edit.add_css_class("flat");
        imp.stop.set_tooltip_text(Some("Stop This Workflow"));
        imp.stop.add_css_class("flat");

        for (button, signal) in [
            (&imp.start, "start"),
            (&imp.edit, "edit"),
            (&imp.stop, "stop"),
        ] {
            button.connect_clicked(clone!(
                #[weak(rename_to = bar)]
                self,
                move |_| bar.emit_by_name::<()>(signal, &[])
            ));
        }

        let strip = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        strip.add_css_class("toolbar");
        strip.add_css_class("workflow-strip");
        strip.append(&imp.summary);
        strip.append(&imp.start);
        strip.append(&imp.edit);
        strip.append(&imp.stop);

        // The strip is the handle for the sheet as well as a toolbar: clicking
        // the label opens the steps, which is what somebody reading "Step 3 of
        // 5" wants next. The buttons keep their own click handlers, so this only
        // fires on the empty part of the row.
        let open = gtk::GestureClick::new();
        open.connect_released(clone!(
            #[weak(rename_to = bar)]
            self,
            move |_, _, _, _| {
                let sheet = &bar.imp().sheet;
                sheet.set_open(!sheet.is_open());
            }
        ));
        imp.summary.add_controller(open);

        imp.goal.set_xalign(0.0);
        imp.goal.set_wrap(true);
        imp.goal.add_css_class("title-4");
        imp.progress.set_xalign(0.0);
        imp.progress.set_wrap(true);
        imp.progress.add_css_class("caption");
        imp.progress.add_css_class("dimmed");

        // Rows are activated for their detail, never selected: there is no
        // "current row" here, and a highlight would say there was.
        imp.steps.set_selection_mode(gtk::SelectionMode::None);
        imp.steps.add_css_class("boxed-list");

        let inside = gtk::Box::new(gtk::Orientation::Vertical, 6);
        inside.append(&imp.goal);
        inside.append(&imp.progress);
        let spaced = gtk::Box::new(gtk::Orientation::Vertical, 12);
        spaced.append(&inside);
        spaced.append(&imp.steps);

        let scroller = gtk::ScrolledWindow::builder()
            .propagate_natural_height(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&spaced)
            .build();
        let sheet_body = adw::Clamp::builder()
            .maximum_size(700)
            .child(&scroller)
            .margin_top(12)
            .margin_bottom(18)
            .margin_start(12)
            .margin_end(12)
            .build();

        imp.sheet.set_bottom_bar(Some(&strip));
        imp.sheet.set_sheet(Some(&sheet_body));
        imp.sheet.set_show_drag_handle(true);
        // You have to be able to read the plan and type at the same time.
        imp.sheet.set_modal(false);
        imp.sheet.set_reveal_bottom_bar(false);
        imp.sheet.set_parent(self);
    }

    /// Draw a workflow, or nothing at all.
    ///
    /// Rebuilt rather than diffed, for the reason the chips are: a handful of
    /// rows, and a row showing the previous step's outcome would be the worst
    /// kind of wrong.
    pub fn set_workflow(&self, workflow: Option<&Workflow>) {
        let imp = self.imp();
        imp.held.replace(workflow.cloned());

        let Some(workflow) = workflow else {
            imp.sheet.set_open(false);
            imp.sheet.set_reveal_bottom_bar(false);
            return;
        };

        imp.summary.set_text(&workflow.summary());
        imp.summary.set_tooltip_text(Some(&workflow.goal));
        // Start is the one thing to do with a plan nobody has greenlit, and it
        // is gone the moment it has been: at most one suggested action in the
        // view, and a button that would repeat what already happened.
        imp.start.set_visible(!workflow.started);
        imp.stop.set_visible(workflow.started);

        imp.goal.set_text(&workflow.goal);
        imp.progress.set_text(&describe(workflow));
        imp.steps.remove_all();
        for (index, step) in workflow.steps.iter().enumerate() {
            imp.steps
                .append(&self.row_for(workflow, index, step.state.clone()));
        }

        imp.sheet.set_reveal_bottom_bar(true);
    }

    /// The workflow currently drawn, so a caller acting on a click does not have
    /// to be handed it back.
    pub fn workflow(&self) -> Option<Workflow> {
        self.imp().held.borrow().clone()
    }

    pub fn set_open(&self, open: bool) {
        self.imp().sheet.set_open(open);
    }

    /// What the collapsed strip says. For the widget tests, which have no
    /// compositor and so cannot read it off the screen.
    pub fn summary_text(&self) -> String {
        self.imp().summary.text().to_string()
    }

    pub fn start_visible(&self) -> bool {
        self.imp().start.get_visible()
    }

    pub fn stop_visible(&self) -> bool {
        self.imp().stop.get_visible()
    }

    /// What each step row's state icon says it is, in order. What the widget
    /// tests read, since they have no compositor to look at.
    pub fn step_states(&self) -> Vec<String> {
        let mut found = Vec::new();
        let mut row = self.imp().steps.first_child();
        while let Some(widget) = row {
            if let Some(state) = first_icon_tooltip(&widget) {
                found.push(state);
            }
            row = widget.next_sibling();
        }
        found
    }

    fn row_for(&self, workflow: &Workflow, index: usize, state: State) -> adw::ActionRow {
        let step = &workflow.steps[index];
        // `started` matters as much as the position. `Workflow::current` is the
        // first unsettled step whether or not anybody has said go, so without
        // this the first step of a proposal wears the accent "doing this now"
        // marker while nothing whatever is happening — which is the one thing a
        // plan awaiting approval must not look like.
        let current = workflow.started && workflow.current() == Some(index) && !workflow.is_stuck();

        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&step.what))
            .title_lines(2)
            .activatable(true)
            .build();

        let (icon, css, tooltip) = marker(&state, current);
        let image = gtk::Image::from_icon_name(icon);
        image.set_tooltip_text(Some(tooltip));
        image.add_css_class(css);
        row.add_prefix(&image);

        // What it produced, under the step, so "done" cannot mask a no-op — the
        // same rule the chips keep.
        let detail = match &state {
            State::Done { outcome } => Some(outcome.clone()),
            State::Skipped { why } | State::Stuck { why } => Some(why.clone()),
            State::Pending => None,
        };
        if let Some(detail) = detail.filter(|text| !text.trim().is_empty()) {
            row.set_subtitle(&glib::markup_escape_text(&detail));
            row.set_subtitle_lines(2);
        }

        // A note is the user's own writing and the only thing here they put in,
        // so it is visible at a glance rather than only inside the row.
        if step.note.is_some() {
            let noted = gtk::Image::from_icon_name("document-edit-symbolic");
            noted.set_tooltip_text(Some("You added a note to this step"));
            noted.add_css_class("dimmed");
            row.add_suffix(&noted);
        }

        row.connect_activated(clone!(
            #[weak(rename_to = bar)]
            self,
            move |row| {
                if let Some(workflow) = bar.workflow() {
                    detail_for(&workflow, index).present(Some(row));
                }
            }
        ));
        row
    }
}

/// The tooltip of the first `GtkImage` anywhere under a widget.
///
/// `AdwActionRow` nests a prefix several boxes deep and the depth is libadwaita's
/// business, so this walks rather than reaching for a known child.
fn first_icon_tooltip(widget: &gtk::Widget) -> Option<String> {
    if let Some(image) = widget.downcast_ref::<gtk::Image>() {
        if let Some(tooltip) = image.tooltip_text() {
            return Some(tooltip.to_string());
        }
    }
    let mut child = widget.first_child();
    while let Some(candidate) = child {
        if let Some(found) = first_icon_tooltip(&candidate) {
            return Some(found);
        }
        child = candidate.next_sibling();
    }
    None
}

/// The icon, style class and tooltip for a step, in the tool chip's vocabulary.
fn marker(state: &State, current: bool) -> (&'static str, &'static str, &'static str) {
    if current {
        return ("content-loading-symbolic", "accent", "Doing this now");
    }
    match state {
        State::Pending => ("radio-symbolic", "dimmed", "Not started"),
        State::Done { .. } => ("object-select-symbolic", "success", "Done"),
        State::Skipped { .. } => ("action-unavailable-symbolic", "dimmed", "Skipped"),
        State::Stuck { .. } => ("dialog-warning-symbolic", "error", "Stuck"),
    }
}

/// The line under the goal in the open sheet.
fn describe(workflow: &Workflow) -> String {
    let (settled, total) = workflow.progress();
    if workflow.is_stuck() {
        return "A step is stuck — the rest is waiting on it.".to_string();
    }
    if workflow.is_finished() {
        return format!("All {total} steps are done.");
    }
    if !workflow.started {
        return "Not started yet. Change anything you want to, then Start.".to_string();
    }
    format!("{settled} of {total} steps done.")
}

/// What one step is hiding: the user's note, and what the step produced.
///
/// A dialog rather than an expander, matching `ui::tool_detail` — a step's
/// outcome is the same kind of thing as a tool's result and there is no reason
/// for the two to open differently.
pub fn detail_for(workflow: &Workflow, index: usize) -> adw::Dialog {
    let step = &workflow.steps[index];
    let dialog = adw::Dialog::builder()
        .title("Step")
        .content_width(480)
        .content_height(420)
        .build();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
    content.set_margin_top(12);
    content.set_margin_bottom(24);
    content.set_margin_start(12);
    content.set_margin_end(12);

    let about = adw::PreferencesGroup::builder()
        .title(glib::markup_escape_text(&step.what))
        .description(format!(
            "Step {} of {} · {}",
            index + 1,
            workflow.steps.len(),
            marker(
                &step.state,
                workflow.started && workflow.current() == Some(index)
            )
            .2
        ))
        .build();
    content.append(&about);

    if let Some(note) = step.note.as_deref().filter(|note| !note.trim().is_empty()) {
        let group = adw::PreferencesGroup::builder()
            .title("Your note")
            .description("Given to the assistant when it reaches this step.")
            .build();
        group.add(&block(note));
        content.append(&group);
    }

    let outcome = match &step.state {
        State::Done { outcome } => Some(("What it produced", outcome.clone())),
        State::Skipped { why } => Some(("Why it was skipped", why.clone())),
        State::Stuck { why } => Some(("Why it is stuck", why.clone())),
        State::Pending => None,
    };
    if let Some((title, text)) = outcome.filter(|(_, text)| !text.trim().is_empty()) {
        let group = adw::PreferencesGroup::builder().title(title).build();
        group.add(&block(&text));
        content.append(&group);
    }

    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(
        &gtk::ScrolledWindow::builder()
            .propagate_natural_height(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&content)
            .build(),
    ));
    dialog.set_child(Some(&view));
    dialog
}

/// A paragraph of text in a boxed list, wrapped rather than ellipsized — this
/// is the thing somebody opened the dialog to read.
fn block(text: &str) -> adw::PreferencesRow {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    label.set_selectable(true);
    label.set_margin_top(12);
    label.set_margin_bottom(12);
    label.set_margin_start(12);
    label.set_margin_end(12);

    let row = adw::PreferencesRow::new();
    row.set_activatable(false);
    row.set_child(Some(&label));
    row
}
