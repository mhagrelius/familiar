//! The window that appears when you press the shortcut.
//!
//! A window of its own rather than a dialog or a panel over the conversation,
//! and the reason is where it is opened from: the shortcut works while Familiar
//! is closed, minimised or on another workspace, so there is no parent to
//! attach to. An `AdwDialog` needs one. This is the one surface in the
//! application that has to stand on its own.
//!
//! It is deliberately not an overlay in the GNOME sense. A window that floats
//! above everything and follows the pointer is a shell extension's job —
//! mutter does not let a client raise or place itself — and pretending
//! otherwise would mean shipping a control that quietly does nothing. This is
//! an ordinary window that mutter centres and focuses, which is what every
//! other GNOME application does when it has something to say.
//!
//! What it shows is the state of the exchange and nothing else: what it heard,
//! what it answered, and which chat that went into. The chat itself is in the
//! main window, where it has always been, and the button in the header goes
//! there.

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib::subclass::Signal;
use gtk::glib::{self, clone};
use std::sync::OnceLock;

use super::State;

/// How many levels the meter remembers. At 40 ms a block, sixty of them is
/// about two and a half seconds of history — enough to read as movement.
const METER_HISTORY: usize = 60;

mod imp {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    pub struct VoiceWindow {
        /// What is happening, in a word.
        pub title: adw::WindowTitle,
        /// Live loudness, so it is obvious the microphone is working.
        pub meter: gtk::DrawingArea,
        /// Shared with the draw function by `Rc`, not by cloning the cell:
        /// cloning a `RefCell` copies what is inside it, so the meter would be
        /// drawn from a snapshot of the history taken before anything was
        /// heard — a row of dots that never moves.
        pub levels: Rc<RefCell<Vec<f64>>>,
        /// What it heard. Dim while the words are still provisional.
        pub heard: gtk::Label,
        pub answer: gtk::Label,
        /// What to do, when there is nothing else on screen to look at.
        pub hint: gtk::Label,
        pub scroller: gtk::ScrolledWindow,
        pub spinner: adw::Spinner,
        /// The primary action, whose meaning is the state it is in.
        pub act: gtk::Button,
        pub cancel: gtk::Button,
        /// Opens the chat this went into, in the main window.
        pub open: gtk::Button,
        pub fresh: gtk::Button,
        pub state: Cell<State>,
    }

    impl Default for VoiceWindow {
        fn default() -> Self {
            Self {
                title: adw::WindowTitle::new("Talk", ""),
                meter: gtk::DrawingArea::new(),
                levels: Rc::new(RefCell::new(Vec::new())),
                heard: gtk::Label::new(None),
                answer: gtk::Label::new(None),
                hint: gtk::Label::new(Some("Press the shortcut and just say it.")),
                scroller: gtk::ScrolledWindow::new(),
                spinner: adw::Spinner::new(),
                act: gtk::Button::new(),
                cancel: gtk::Button::with_label("Cancel"),
                open: gtk::Button::from_icon_name("go-next-symbolic"),
                fresh: gtk::Button::from_icon_name("list-add-symbolic"),
                state: Cell::new(State::Idle),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for VoiceWindow {
        const NAME: &'static str = "FamiliarVoiceWindow";
        type Type = super::VoiceWindow;
        type ParentType = adw::Window;
    }

    impl ObjectImpl for VoiceWindow {
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![
                    // The primary button, which means whatever the state means:
                    // start listening, send early, or stop.
                    Signal::builder("act").build(),
                    // Throw this exchange away.
                    Signal::builder("cancel").build(),
                    // Put the next question in a chat of its own.
                    Signal::builder("fresh").build(),
                    // Show the chat this went into.
                    Signal::builder("open").build(),
                ]
            })
        }

        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_title(Some("Talk to Familiar"));
            obj.set_default_size(460, 300);
            // Reused rather than rebuilt: the shortcut is pressed often and a
            // window that is constructed each time loses its size and flickers.
            obj.set_hide_on_close(true);

            let header = adw::HeaderBar::new();
            header.set_title_widget(Some(&self.title));
            self.fresh.set_tooltip_text(Some("Start a New Chat"));
            self.fresh.add_css_class("flat");
            header.pack_start(&self.fresh);
            self.open.set_tooltip_text(Some("Show This Chat"));
            self.open.add_css_class("flat");
            header.pack_end(&self.open);

            self.meter.set_content_height(30);
            self.meter.add_css_class("voice-meter");
            self.meter.set_margin_top(6);

            self.heard.set_wrap(true);
            self.heard.set_xalign(0.0);
            self.heard.set_selectable(true);
            self.heard.add_css_class("title-4");

            self.answer.set_wrap(true);
            self.answer.set_xalign(0.0);
            self.answer.set_selectable(true);

            self.hint.add_css_class("dimmed");
            self.hint.set_wrap(true);
            self.hint.set_justify(gtk::Justification::Center);
            self.hint.set_vexpand(true);
            self.hint.set_valign(gtk::Align::Center);

            let said = gtk::Box::new(gtk::Orientation::Vertical, 12);
            said.append(&self.hint);
            said.append(&self.heard);
            said.append(&self.spinner);
            said.append(&self.answer);
            self.spinner.set_halign(gtk::Align::Start);
            self.spinner.set_visible(false);

            self.scroller.set_child(Some(&said));
            self.scroller.set_vexpand(true);
            self.scroller.set_hscrollbar_policy(gtk::PolicyType::Never);
            self.scroller.set_propagate_natural_height(true);

            let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
            content.set_margin_top(6);
            content.set_margin_bottom(12);
            content.set_margin_start(18);
            content.set_margin_end(18);
            content.append(&self.meter);
            content.append(&self.scroller);

            let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            buttons.set_halign(gtk::Align::End);
            self.act.add_css_class("suggested-action");
            self.act.add_css_class("pill");
            self.cancel.add_css_class("pill");
            buttons.append(&self.cancel);
            buttons.append(&self.act);
            content.append(&buttons);

            // Enter presses the primary button, whatever it currently says.
            // Without this the window answers no key but Escape, and the only
            // way to send early is the mouse or the global shortcut.
            self.act.set_receives_default(true);

            let view = adw::ToolbarView::new();
            view.add_top_bar(&header);
            view.set_content(Some(&content));
            obj.set_content(Some(&view));
            obj.set_default_widget(Some(&self.act));

            self.act.connect_clicked(clone!(
                #[weak]
                obj,
                move |_| obj.emit_by_name::<()>("act", &[])
            ));
            self.cancel.connect_clicked(clone!(
                #[weak]
                obj,
                move |_| obj.emit_by_name::<()>("cancel", &[])
            ));
            self.fresh.connect_clicked(clone!(
                #[weak]
                obj,
                move |_| obj.emit_by_name::<()>("fresh", &[])
            ));
            self.open.connect_clicked(clone!(
                #[weak]
                obj,
                move |_| obj.emit_by_name::<()>("open", &[])
            ));

            // Escape is what everybody presses to make a thing go away, and
            // here it means the same as Cancel.
            let escape = gtk::EventControllerKey::new();
            escape.connect_key_pressed(clone!(
                #[weak]
                obj,
                #[upgrade_or]
                glib::Propagation::Proceed,
                move |_, key, _, _| {
                    if key == gtk::gdk::Key::Escape {
                        obj.emit_by_name::<()>("cancel", &[]);
                        return glib::Propagation::Stop;
                    }
                    glib::Propagation::Proceed
                }
            ));
            obj.add_controller(escape);

            obj.show_hint();

            let levels = self.levels.clone();
            self.meter.set_draw_func(move |area, cairo, width, height| {
                draw_meter(area, cairo, width, height, &levels.borrow());
            });
        }
    }

    impl WidgetImpl for VoiceWindow {}
    impl WindowImpl for VoiceWindow {}
    impl AdwWindowImpl for VoiceWindow {}

    /// The level history, newest at the right.
    ///
    /// Bars rather than a line: a bar per block is the same shape as every
    /// recording indicator anybody has seen, and it stays readable at 30 px.
    /// The colour is the widget's own, which `.voice-meter` sets from the
    /// accent variable, so it follows the theme rather than naming a colour.
    ///
    /// Every slot is drawn whether or not there is a level for it yet, so the
    /// meter is a full-width row of dots the moment it appears and fills from
    /// the right as somebody talks. Drawing only what has been heard leaves it
    /// blank for the first two seconds of every utterance, which is exactly
    /// when somebody is looking at it to find out whether they are being
    /// heard.
    fn draw_meter(
        area: &gtk::DrawingArea,
        cairo: &gtk::cairo::Context,
        width: i32,
        height: i32,
        levels: &[f64],
    ) {
        let colour = area.color();
        cairo.set_source_rgba(
            colour.red() as f64,
            colour.green() as f64,
            colour.blue() as f64,
            colour.alpha() as f64,
        );
        let width = width as f64;
        let height = height as f64;
        let slot = width / METER_HISTORY as f64;
        let bar = (slot * 0.5).max(2.0);
        for index in 0..METER_HISTORY {
            // The newest level goes in the rightmost slot.
            let age = METER_HISTORY - 1 - index;
            let level = levels
                .len()
                .checked_sub(age + 1)
                .and_then(|at| levels.get(at))
                .copied()
                .unwrap_or(0.0);
            // A floor under every bar, so silence is a line of dots rather
            // than a gap.
            let tall = (height * level.clamp(0.0, 1.0)).max(bar);
            let x = index as f64 * slot + (slot - bar) / 2.0;
            cairo.rectangle(x, (height - tall) / 2.0, bar, tall);
        }
        let _ = cairo.fill();
    }
}

glib::wrapper! {
    pub struct VoiceWindow(ObjectSubclass<imp::VoiceWindow>)
        @extends adw::Window, gtk::Window, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Native,
                    gtk::Root, gtk::ShortcutManager;
}

impl Default for VoiceWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl VoiceWindow {
    pub fn new() -> Self {
        glib::Object::new()
    }

    /// Add one block's loudness to the meter.
    pub fn hear(&self, level: f64) {
        let imp = self.imp();
        {
            let mut levels = imp.levels.borrow_mut();
            levels.push(level);
            if levels.len() > METER_HISTORY {
                let excess = levels.len() - METER_HISTORY;
                levels.drain(..excess);
            }
        }
        imp.meter.queue_draw();
    }

    fn clear_meter(&self) {
        self.imp().levels.borrow_mut().clear();
        self.imp().meter.queue_draw();
    }

    /// The words so far. `settled` is false while they may still change.
    pub fn set_heard(&self, text: &str, settled: bool) {
        let imp = self.imp();
        imp.heard.set_text(text);
        imp.heard.set_visible(!text.trim().is_empty());
        if settled {
            imp.heard.remove_css_class("dimmed");
        } else {
            imp.heard.add_css_class("dimmed");
        }
        self.show_hint();
    }

    pub fn set_answer(&self, text: &str) {
        let imp = self.imp();
        imp.answer.set_text(text);
        imp.answer.set_visible(!text.trim().is_empty());
        self.show_hint();
        self.follow();
    }

    /// The hint is what fills an otherwise empty window, so it is up exactly
    /// when there is nothing else to read.
    fn show_hint(&self) {
        let imp = self.imp();
        let empty = imp.heard.text().trim().is_empty() && imp.answer.text().trim().is_empty();
        imp.hint.set_visible(empty);
    }

    /// Keep the newest text in view as the answer grows.
    fn follow(&self) {
        let adjustment = self.imp().scroller.vadjustment();
        // After the label has been laid out, or the value being scrolled to is
        // the height the text had before this delta.
        glib::idle_add_local_once(move || {
            adjustment.set_value(adjustment.upper() - adjustment.page_size());
        });
    }

    /// Which chat this is going into, if it is continuing one.
    pub fn set_chat(&self, title: Option<&str>) {
        let imp = self.imp();
        match title {
            Some(title) => imp.title.set_subtitle(&format!("Carrying on “{title}”")),
            None => imp.title.set_subtitle("A new chat"),
        }
        // Nothing to open until there is a chat to open.
        imp.open.set_sensitive(title.is_some());
    }

    pub fn state(&self) -> State {
        self.imp().state.get()
    }

    /// Put the window into a state. This is the only thing that names buttons.
    pub fn set_state(&self, state: State) {
        let imp = self.imp();
        imp.state.set(state);
        let (title, act, act_suggested) = match state {
            State::Idle => ("Talk", "Talk", true),
            State::Listening => ("Listening", "Send", true),
            State::Transcribing => ("Getting that down", "Stop", false),
            State::Thinking => ("Thinking", "Stop", false),
            State::Speaking => ("Speaking", "Stop", false),
        };
        imp.title.set_title(title);
        imp.act.set_label(act);
        if act_suggested {
            imp.act.add_css_class("suggested-action");
        } else {
            imp.act.remove_css_class("suggested-action");
        }
        imp.meter.set_visible(matches!(state, State::Listening));
        imp.spinner
            .set_visible(matches!(state, State::Transcribing | State::Thinking));
        // Cancel means "throw this away", which is only a thing while there is
        // something to throw away.
        imp.cancel.set_visible(!matches!(state, State::Idle));
        // A new chat cannot be started underneath a running turn.
        imp.fresh
            .set_sensitive(matches!(state, State::Idle | State::Listening));
        if matches!(state, State::Listening) {
            self.clear_meter();
        }
    }

    /// Say why nothing can be heard. The window stays; the state does not lie.
    pub fn set_trouble(&self, trouble: &str) {
        let imp = self.imp();
        imp.answer.set_text(trouble);
        imp.answer.set_visible(true);
        imp.answer.add_css_class("warning");
    }

    /// Start an exchange from nothing.
    pub fn reset(&self) {
        let imp = self.imp();
        imp.answer.remove_css_class("warning");
        self.set_heard("", false);
        self.set_answer("");
        self.clear_meter();
    }
}
