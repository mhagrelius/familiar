//! The window: a sidebar of projects and their chats, a conversation, and a
//! composer.
//!
//! It builds the tree and forwards what the user did to the application, which
//! is the only thing that acts on it. Nothing here reads or writes a file, and
//! nothing here talks to the server.

use std::sync::OnceLock;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib::subclass::Signal;
use gtk::glib::{self, clone};

use crate::model::project::{Project, ThreadSummary};
use crate::model::thread::ThreadId;
use crate::ui::{Composer, Conversation, ProjectView, Sidebar, Turn, WorkflowBar};

mod imp {
    use super::*;

    pub struct Window {
        /// The part of the primary menu that acts on the open project. Rebuilt
        /// when the project changes, because what it can offer depends on
        /// whether there is a project at all.
        pub project_menu: gio::Menu,
        pub split: adw::OverlaySplitView,
        pub sidebar: Sidebar,
        pub conversation: Conversation,
        /// Wrapped *around* the conversation and not around the composer, so
        /// its strip sits between the two — where somebody about to type a
        /// correction is already looking.
        pub workflow: WorkflowBar,
        pub composer: Composer,
        /// A project's own page, shown in place of the conversation.
        pub project: ProjectView,
        /// Which of the two the content side is showing.
        pub pages: gtk::Stack,
        pub banner: adw::Banner,
        pub toasts: adw::ToastOverlay,
        pub title: adw::WindowTitle,
        pub status: gtk::Label,
        pub usage: gtk::LevelBar,
    }

    impl Default for Window {
        fn default() -> Self {
            Self {
                project_menu: gio::Menu::new(),
                split: adw::OverlaySplitView::new(),
                sidebar: Sidebar::new(),
                conversation: Conversation::new(),
                workflow: WorkflowBar::new(),
                composer: Composer::new(),
                project: ProjectView::new(),
                pages: gtk::Stack::new(),
                banner: adw::Banner::new(""),
                toasts: adw::ToastOverlay::new(),
                title: adw::WindowTitle::new("Familiar", ""),
                status: gtk::Label::new(None),
                usage: gtk::LevelBar::new(),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Window {
        const NAME: &'static str = "FamiliarWindow";
        type Type = super::Window;
        type ParentType = adw::ApplicationWindow;
    }

    impl ObjectImpl for Window {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().build();
        }

        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![
                    Signal::builder("submit")
                        .param_types([String::static_type()])
                        .build(),
                    Signal::builder("stop").build(),
                    // project slug
                    Signal::builder("new-thread")
                        .param_types([String::static_type()])
                        .build(),
                    // project slug, chat id
                    Signal::builder("thread-chosen")
                        .param_types([String::static_type(), String::static_type()])
                        .build(),
                    Signal::builder("thread-rename")
                        .param_types([String::static_type(), String::static_type()])
                        .build(),
                    Signal::builder("thread-delete")
                        .param_types([String::static_type(), String::static_type()])
                        .build(),
                    // what to do, and to which project: "open", "edit", "delete"
                    Signal::builder("project-action")
                        .param_types([String::static_type(), String::static_type()])
                        .build(),
                    // the same, from the open project's own page, which needs
                    // no slug because the application knows whose page it is
                    Signal::builder("page-project-action")
                        .param_types([String::static_type()])
                        .build(),
                    // a chat picked from that page
                    Signal::builder("page-chat-chosen")
                        .param_types([String::static_type()])
                        .build(),
                    // what to do, and to which path
                    Signal::builder("file-action")
                        .param_types([String::static_type(), String::static_type()])
                        .build(),
                    Signal::builder("retry").build(),
                    Signal::builder("complain")
                        .param_types([String::static_type()])
                        .build(),
                ]
            })
        }
    }

    impl WidgetImpl for Window {}
    impl WindowImpl for Window {}
    impl ApplicationWindowImpl for Window {}
    impl AdwApplicationWindowImpl for Window {}
}

glib::wrapper! {
    pub struct Window(ObjectSubclass<imp::Window>)
        @extends adw::ApplicationWindow, gtk::ApplicationWindow, gtk::Window, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Native,
                    gtk::Root, gtk::ShortcutManager, gio::ActionGroup, gio::ActionMap;
}

impl Window {
    pub fn new(application: &impl IsA<gtk::Application>) -> Self {
        glib::Object::builder()
            .property("application", application)
            .build()
    }

    fn build(&self) {
        let imp = self.imp();

        self.set_title(Some("Familiar"));
        self.set_default_size(1000, 720);

        // -- the sidebar side ------------------------------------------------
        // One button that starts a chat, with the less common thing it could
        // start hanging off it. A second header button for "New Project" would
        // be two affordances for one idea; a split button is the pattern for
        // exactly this.
        let more = gio::Menu::new();
        more.append(Some("New Project…"), Some("win.new-project"));
        let new_chat = adw::SplitButton::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("New Chat")
            .dropdown_tooltip("Start Something Else")
            .action_name("win.new-thread")
            .menu_model(&more)
            .build();

        let sidebar_header = adw::HeaderBar::new();
        sidebar_header.set_title_widget(Some(&adw::WindowTitle::new("Familiar", "")));
        sidebar_header.pack_end(&new_chat);

        let sidebar_side = adw::ToolbarView::new();
        sidebar_side.add_top_bar(&sidebar_header);
        sidebar_side.set_content(Some(&imp.sidebar));

        // -- the conversation side -------------------------------------------
        let toggle = gtk::ToggleButton::new();
        toggle.set_icon_name("sidebar-show-symbolic");
        toggle.set_tooltip_text(Some("Toggle Sidebar"));
        // Bound to the property, not to the action: with both, a click would
        // toggle the button *and* fire the action, and the sidebar would end
        // up exactly where it started.
        imp.split
            .bind_property("show-sidebar", &toggle, "active")
            .sync_create()
            .bidirectional()
            .build();

        let thread_section = gio::Menu::new();
        thread_section.append(Some("New Chat"), Some("win.new-thread"));
        thread_section.append(Some("Rename Chat…"), Some("win.rename-thread"));
        thread_section.append(Some("Delete Chat…"), Some("win.delete-thread"));
        thread_section.append(Some("Schedule…"), Some("win.schedule-thread"));

        let app_section = gio::Menu::new();
        app_section.append(Some("Scheduled Chats…"), Some("app.schedules"));
        app_section.append(Some("Preferences"), Some("app.preferences"));
        app_section.append(Some("Keyboard Shortcuts"), Some("win.shortcuts"));
        app_section.append(Some("About Familiar"), Some("app.about"));

        let menu = gio::Menu::new();
        menu.append_section(None, &thread_section);
        menu.append_section(None, &imp.project_menu);
        menu.append_section(None, &app_section);
        let menu_button = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .tooltip_text("Main Menu")
            .menu_model(&menu)
            .build();

        let header = adw::HeaderBar::new();
        header.pack_start(&toggle);
        header.pack_end(&menu_button);
        header.set_title_widget(Some(&imp.title));

        imp.banner.set_button_label(Some("Retry"));
        imp.banner.set_revealed(false);
        imp.banner.connect_button_clicked(clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.emit_by_name::<()>("retry", &[])
        ));

        // The bottom bar: what model, and how full its context is.
        imp.status.add_css_class("caption");
        imp.status.add_css_class("dimmed");
        imp.status.set_xalign(0.0);
        imp.status.set_hexpand(true);
        imp.status.set_ellipsize(gtk::pango::EllipsizeMode::End);

        imp.usage.set_valign(gtk::Align::Center);
        imp.usage.set_width_request(120);
        imp.usage.set_visible(false);

        let bottom = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        bottom.add_css_class("toolbar");
        bottom.append(&imp.status);
        bottom.append(&imp.usage);

        imp.workflow.set_content(&imp.conversation);
        imp.workflow.set_vexpand(true);

        let chat = gtk::Box::new(gtk::Orientation::Vertical, 0);
        chat.append(&imp.banner);
        chat.append(&imp.workflow);
        chat.append(&imp.composer);

        // Two things the content side can be: the conversation, or the page of
        // the project you clicked. The composer belongs to the first — there is
        // nothing on a project page to type into.
        imp.pages.add_named(&chat, Some("chat"));
        imp.pages.add_named(&imp.project, Some("project"));
        imp.pages.set_vexpand(true);

        let conversation_side = adw::ToolbarView::new();
        conversation_side.add_top_bar(&header);
        conversation_side.set_content(Some(&imp.pages));
        conversation_side.add_bottom_bar(&bottom);

        imp.split.set_sidebar(Some(&sidebar_side));
        imp.split.set_content(Some(&conversation_side));
        imp.split.set_max_sidebar_width(300.0);

        imp.toasts.set_child(Some(&imp.split));
        self.set_content(Some(&imp.toasts));

        // Narrow enough that the sidebar would crowd the conversation: it
        // becomes an overlay instead of a column.
        let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            675.0,
            adw::LengthUnit::Sp,
        ));
        breakpoint.add_setter(&imp.split, "collapsed", Some(&true.to_value()));
        self.add_breakpoint(breakpoint);

        // -- forwarding -------------------------------------------------------
        imp.composer.connect_closure(
            "submit",
            false,
            glib::closure_local!(
                #[watch(rename_to = window)]
                self,
                move |_: Composer, text: String| {
                    window.emit_by_name::<()>("submit", &[&text]);
                }
            ),
        );
        imp.composer.connect_closure(
            "complain",
            false,
            glib::closure_local!(
                #[watch(rename_to = window)]
                self,
                move |_: Composer, text: String| {
                    window.emit_by_name::<()>("complain", &[&text]);
                }
            ),
        );
        // Explaining a selection is asking a question, so it leaves here as
        // one. Nothing downstream needs to know it was not typed.
        imp.conversation.connect_closure(
            "explain",
            false,
            glib::closure_local!(
                #[watch(rename_to = window)]
                self,
                move |_: Conversation, question: String| {
                    window.emit_by_name::<()>("submit", &[&question]);
                }
            ),
        );
        imp.composer.connect_closure(
            "stop",
            false,
            glib::closure_local!(
                #[watch(rename_to = window)]
                self,
                move |_: Composer| window.emit_by_name::<()>("stop", &[])
            ),
        );
        // The sidebar's signals are the window's signals: it reports intent and
        // the application acts on it.
        for name in [
            "thread-chosen",
            "thread-rename",
            "thread-delete",
            "project-action",
        ] {
            imp.sidebar.connect_closure(
                name,
                false,
                glib::closure_local!(
                    #[watch(rename_to = window)]
                    self,
                    move |_: Sidebar, first: String, second: String| {
                        window.emit_by_name::<()>(name, &[&first, &second]);
                    }
                ),
            );
        }
        imp.sidebar.connect_closure(
            "new-thread",
            false,
            glib::closure_local!(
                #[watch(rename_to = window)]
                self,
                move |_: Sidebar, slug: String| {
                    window.emit_by_name::<()>("new-thread", &[&slug]);
                }
            ),
        );
        // The project page's buttons are the window's signals too. It does not
        // know which project it is showing — the application put it there and
        // has not forgotten — so what leaves here carries no slug.
        imp.project.connect_closure(
            "chat-chosen",
            false,
            glib::closure_local!(
                #[watch(rename_to = window)]
                self,
                move |_: ProjectView, id: String| {
                    window.emit_by_name::<()>("page-chat-chosen", &[&id]);
                }
            ),
        );
        for (from, action) in [("edit-project", "edit"), ("choose-folder", "folder")] {
            imp.project.connect_closure(
                from,
                false,
                glib::closure_local!(
                    #[watch(rename_to = window)]
                    self,
                    move |_: ProjectView| {
                        window.emit_by_name::<()>("page-project-action", &[&action.to_string()]);
                    }
                ),
            );
        }
        imp.project.connect_closure(
            "file-action",
            false,
            glib::closure_local!(
                #[watch(rename_to = window)]
                self,
                move |_: ProjectView, action: String, path: String| {
                    window.emit_by_name::<()>("file-action", &[&action, &path]);
                }
            ),
        );

        // The project section has to say something before the application has
        // told it which project is open, and at that point it is the default
        // one by construction.
        self.set_project("", true);

        // F9, the GNOME convention for the sidebar.
        let sidebar_action = gio::SimpleAction::new("toggle-sidebar", None);
        sidebar_action.connect_activate(clone!(
            #[weak(rename_to = window)]
            self,
            move |_, _| {
                let split = &window.imp().split;
                split.set_show_sidebar(!split.shows_sidebar());
            }
        ));
        self.add_action(&sidebar_action);

        // The keyboard half of "Explain This". The menu item goes through the
        // turn's own action group, but a shortcut arrives at the window with
        // no idea which answer it means — so it asks the focused widget, which
        // is the view you just dragged a selection across.
        let explain = gio::SimpleAction::new("explain-selection", None);
        explain.connect_activate(clone!(
            #[weak(rename_to = window)]
            self,
            move |_, _| {
                let asked = gtk::prelude::GtkWindowExt::focus(&window)
                    .and_then(|focus| focus.ancestor(Turn::static_type()))
                    .and_downcast::<Turn>()
                    .is_some_and(|turn| turn.explain());
                if !asked {
                    window.toast("Select something in an answer first");
                }
            }
        ));
        self.add_action(&explain);

        // Escape stops a turn; it is the one key a person tries first.
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(clone!(
            #[weak(rename_to = window)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, key, _, _| {
                if key == gtk::gdk::Key::Escape {
                    window.emit_by_name::<()>("stop", &[]);
                }
                glib::Propagation::Proceed
            }
        ));
        self.add_controller(keys);
    }

    pub fn conversation(&self) -> Conversation {
        self.imp().conversation.clone()
    }

    pub fn composer(&self) -> Composer {
        self.imp().composer.clone()
    }

    pub fn set_projects(&self, projects: &[(Project, Vec<ThreadSummary>)]) {
        self.imp().sidebar.set_projects(projects);
    }

    pub fn select_thread(&self, slug: &str, id: Option<&ThreadId>) {
        self.imp().sidebar.select(slug, id);
    }

    /// Show a project's page in place of the conversation.
    ///
    /// The title bar becomes the project's name with nothing under it: the page
    /// *is* the project, and saying which project it belongs to underneath its
    /// own name is the same word twice.
    pub fn show_project_page(
        &self,
        project: &crate::model::project::Project,
        chats: &[ThreadSummary],
        scheduled: &[crate::ui::dialogs::Scheduled],
    ) {
        let imp = self.imp();
        imp.project.set_project(project, chats, scheduled);
        imp.pages.set_visible_child_name("project");
        imp.title.set_title(&project.name);
        imp.title.set_subtitle("");
    }

    /// Back to the conversation.
    pub fn show_chat(&self) {
        self.imp().pages.set_visible_child_name("chat");
    }

    /// Whether the project page is the thing on screen.
    pub fn showing_project(&self) -> bool {
        self.imp()
            .pages
            .visible_child_name()
            .is_some_and(|name| name == "project")
    }

    pub fn project_view(&self) -> ProjectView {
        self.imp().project.clone()
    }

    pub fn workflow_bar(&self) -> WorkflowBar {
        self.imp().workflow.clone()
    }

    pub fn set_thread_title(&self, title: &str) {
        self.imp().title.set_title(title);
    }

    /// The bottom bar: the model, and how much of its context this chat uses.
    pub fn set_status(&self, text: &str) {
        self.imp().status.set_text(text);
    }

    pub fn set_context_usage(&self, fraction: Option<f64>) {
        let usage = &self.imp().usage;
        match fraction {
            Some(fraction) => {
                usage.set_value(fraction.clamp(0.0, 1.0));
                usage.set_visible(true);
            }
            None => usage.set_visible(false),
        }
    }

    /// An ongoing condition, so it is a banner and it stays up until it is
    /// fixed — not a toast that is missed while typing.
    pub fn set_trouble(&self, text: Option<&str>) {
        let banner = &self.imp().banner;
        match text {
            Some(text) => {
                banner.set_title(text);
                banner.set_revealed(true);
            }
            None => banner.set_revealed(false),
        }
    }

    pub fn toast(&self, text: &str) {
        self.imp().toasts.add_toast(adw::Toast::new(text));
    }

    /// A toast the caller built, because it carries an Undo button.
    pub fn present_toast(&self, toast: &adw::Toast) {
        self.imp().toasts.add_toast(toast.clone());
    }

    /// Which project the conversation belongs to.
    ///
    /// Its name goes under the chat's title — without it there is no way to
    /// tell two similarly named chats apart — and the menu that acts on it is
    /// rebuilt, because "Delete Project" means nothing when the answer to
    /// "which project?" is that there isn't one.
    pub fn set_project(&self, name: &str, default: bool) {
        let imp = self.imp();
        imp.title.set_subtitle(name);

        let menu = &imp.project_menu;
        menu.remove_all();
        menu.append(Some("New Project…"), Some("win.new-project"));
        // The page is opened by clicking the project in the sidebar, which is
        // the gesture; this is the same thing from the keyboard.
        menu.append(Some("Overview"), Some("win.open-project"));
        menu.append(
            Some(if default {
                "Chat Settings…"
            } else {
                "Project Settings…"
            }),
            Some("win.edit-project"),
        );
        if !default {
            menu.append(Some("Delete Project…"), Some("win.delete-project"));
        }
    }
}
