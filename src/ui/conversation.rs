//! The scroller of turns.
//!
//! A `GtkBox` in a scroller under an `AdwClamp`, not a `GtkListView`. Recycled
//! rows fight variable-height streaming text, and compaction bounds how many
//! turns a thread carries anyway — so the simple thing is also the right one
//! here.
//!
//! Autoscroll follows the answer only while you are already at the bottom.
//! Scrolling up to re-read something and being yanked back down every 50 ms is
//! the worst thing a streaming UI can do to you.

use std::sync::OnceLock;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib::subclass::Signal;
use gtk::glib::{self, clone};

use crate::ui::Turn;

/// How close to the end still counts as "at the bottom", in pixels.
const AT_BOTTOM: f64 = 64.0;

mod imp {
    use super::*;
    use std::cell::Cell;

    pub struct Conversation {
        pub stack: gtk::Stack,
        pub scroller: gtk::ScrolledWindow,
        pub turns: gtk::Box,
        pub empty: adw::StatusPage,
        /// Set while a turn is streaming, so the view follows it down.
        pub following: Cell<bool>,
    }

    impl Default for Conversation {
        fn default() -> Self {
            Self {
                stack: gtk::Stack::new(),
                scroller: gtk::ScrolledWindow::new(),
                turns: gtk::Box::new(gtk::Orientation::Vertical, 24),
                empty: adw::StatusPage::new(),
                following: Cell::new(true),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Conversation {
        const NAME: &'static str = "FamiliarConversation";
        type Type = super::Conversation;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_layout_manager_type::<gtk::BinLayout>();
        }
    }

    impl ObjectImpl for Conversation {
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
                // One of the turns wants to be asked about; the question is
                // already written.
                vec![Signal::builder("explain")
                    .param_types([String::static_type()])
                    .build()]
            })
        }
    }

    impl WidgetImpl for Conversation {}
}

glib::wrapper! {
    pub struct Conversation(ObjectSubclass<imp::Conversation>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for Conversation {
    fn default() -> Self {
        Self::new()
    }
}

impl Conversation {
    pub fn new() -> Self {
        glib::Object::new()
    }

    fn build(&self) {
        let imp = self.imp();

        imp.turns.set_margin_top(24);
        imp.turns.set_margin_bottom(24);
        imp.turns.set_margin_start(12);
        imp.turns.set_margin_end(12);

        // Prose set to the full width of a maximised window loses the eye
        // between lines. This is what GNOME apps showing a document do.
        let clamp = adw::Clamp::builder()
            .maximum_size(760)
            .tightening_threshold(600)
            .child(&imp.turns)
            .build();

        imp.scroller.set_hscrollbar_policy(gtk::PolicyType::Never);
        imp.scroller.set_vexpand(true);
        imp.scroller.set_child(Some(&clamp));

        // Any deliberate scroll away from the bottom stops the follow; coming
        // back to the bottom resumes it.
        let adjustment = imp.scroller.vadjustment();
        adjustment.connect_value_changed(clone!(
            #[weak(rename_to = conversation)]
            self,
            move |adjustment| {
                conversation.imp().following.set(at_bottom(adjustment));
            }
        ));

        imp.empty.set_icon_name(Some("chat-message-new-symbolic"));
        imp.empty.set_title("Ask Something");
        imp.empty
            .set_description(Some("The model runs on this machine."));

        imp.stack.add_named(&imp.empty, Some("empty"));
        imp.stack.add_named(&imp.scroller, Some("turns"));
        imp.stack.set_visible_child_name("empty");
        imp.stack.set_parent(self);
        self.set_vexpand(true);
    }

    pub fn append(&self, turn: &Turn) {
        let imp = self.imp();
        // Every turn on screen can be asked about, replayed ones included —
        // the answer worth digging into is usually not the one still arriving.
        turn.connect_closure(
            "explain",
            false,
            glib::closure_local!(
                #[watch(rename_to = conversation)]
                self,
                move |_: Turn, question: String| {
                    conversation.emit_by_name::<()>("explain", &[&question]);
                }
            ),
        );
        imp.turns.append(turn);
        imp.stack.set_visible_child_name("turns");
        imp.following.set(true);
        self.scroll_to_end();
    }

    pub fn clear(&self) {
        let imp = self.imp();
        while let Some(child) = imp.turns.first_child() {
            imp.turns.remove(&child);
        }
        imp.stack.set_visible_child_name("empty");
        imp.following.set(true);
    }

    /// Follow a growing answer, if the reader has not scrolled away.
    pub fn follow(&self) {
        if self.imp().following.get() {
            self.scroll_to_end();
        }
    }

    fn scroll_to_end(&self) {
        // On idle: the new text has not been laid out yet, so the adjustment's
        // upper bound is still the old one until GTK has measured it.
        glib::idle_add_local_once(clone!(
            #[weak(rename_to = conversation)]
            self,
            move || {
                let adjustment = conversation.imp().scroller.vadjustment();
                adjustment.set_value(adjustment.upper() - adjustment.page_size());
            }
        ));
    }
}

fn at_bottom(adjustment: &gtk::Adjustment) -> bool {
    adjustment.value() + adjustment.page_size() >= adjustment.upper() - AT_BOTTOM
}
