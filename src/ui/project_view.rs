//! A project's own page: what it is, what is in it, and what it is doing.
//!
//! Opened by clicking the project in the sidebar, in place of the conversation.
//! Before this existed, a project was only ever a heading with chats under it —
//! its folder was a cramped branch in a 300-pixel sidebar, its instructions were
//! two menus away, and the fact that one of its chats woke up every morning was
//! visible nowhere at all.
//!
//! Four things, in the order somebody coming back to a project wants them:
//! what it has been told to do, the files it is about, the chats in it, and
//! anything that runs on its own.
//!
//! It renders and reports. Every button leaves as a signal.

use std::cell::RefCell;
use std::path::Path;
use std::sync::OnceLock;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib::subclass::Signal;
use gtk::glib::{self, clone};

use crate::model::project::{Project, ThreadSummary};
use crate::ui::dialogs::Scheduled;
use crate::ui::FileTree;

mod imp {
    use super::*;

    pub struct ProjectView {
        pub content: gtk::Box,
        pub files: FileTree,
        /// What is being shown, so a search can refilter without the caller
        /// handing it all over again.
        pub chats: RefCell<Vec<ThreadSummary>>,
        pub slug: RefCell<String>,
        pub query: RefCell<String>,
        pub chat_list: RefCell<Option<gtk::ListBox>>,
    }

    impl Default for ProjectView {
        fn default() -> Self {
            Self {
                content: gtk::Box::new(gtk::Orientation::Vertical, 24),
                files: FileTree::new(),
                chats: RefCell::new(Vec::new()),
                slug: RefCell::new(String::new()),
                query: RefCell::new(String::new()),
                chat_list: RefCell::new(None),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ProjectView {
        const NAME: &'static str = "FamiliarProjectView";
        type Type = super::ProjectView;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_layout_manager_type::<gtk::BinLayout>();
        }
    }

    impl ObjectImpl for ProjectView {
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
                    // chat id
                    Signal::builder("chat-chosen")
                        .param_types([String::static_type()])
                        .build(),
                    Signal::builder("edit-project").build(),
                    Signal::builder("choose-folder").build(),
                    // what to do, and to which path
                    Signal::builder("file-action")
                        .param_types([String::static_type(), String::static_type()])
                        .build(),
                ]
            })
        }
    }

    impl WidgetImpl for ProjectView {}
}

glib::wrapper! {
    pub struct ProjectView(ObjectSubclass<imp::ProjectView>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for ProjectView {
    fn default() -> Self {
        Self::new()
    }
}

impl ProjectView {
    pub fn new() -> Self {
        glib::Object::new()
    }

    fn build(&self) {
        let imp = self.imp();
        imp.content.set_margin_top(24);
        imp.content.set_margin_bottom(36);
        imp.content.set_margin_start(12);
        imp.content.set_margin_end(12);

        imp.files.connect_closure(
            "file-action",
            false,
            glib::closure_local!(
                #[watch(rename_to = view)]
                self,
                move |_: FileTree, action: String, path: String| {
                    view.emit_by_name::<()>("file-action", &[&action, &path]);
                }
            ),
        );

        let clamp = adw::Clamp::builder()
            .maximum_size(760)
            .child(&imp.content)
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&clamp)
            .build();
        scroller.set_parent(self);
        self.set_vexpand(true);
        self.set_hexpand(true);
    }

    /// Draw the page for one project.
    ///
    /// Rebuilt whole rather than diffed: it is shown when somebody asks for it
    /// and refreshed between turns, so it is never redrawn faster than a person
    /// can read, and a diff that is subtly wrong shows a file that is not there.
    pub fn set_project(&self, project: &Project, chats: &[ThreadSummary], scheduled: &[Scheduled]) {
        let imp = self.imp();
        imp.slug.replace(project.slug.clone());
        imp.chats.replace(chats.to_vec());
        imp.query.replace(String::new());

        while let Some(child) = imp.content.first_child() {
            imp.content.remove(&child);
        }

        imp.content.append(&self.heading(project, chats));
        imp.content.append(&self.instructions(project));
        imp.content.append(&self.files_group(project));
        imp.content.append(&self.chats_group(chats));
        if !scheduled.is_empty() {
            imp.content.append(&self.scheduled_group(scheduled));
        }
    }

    // -- the parts ------------------------------------------------------------

    fn heading(&self, project: &Project, chats: &[ThreadSummary]) -> gtk::Widget {
        let title = gtk::Label::builder()
            .label(&project.name)
            .xalign(0.0)
            .wrap(true)
            .build();
        title.add_css_class("title-1");

        // The count and nothing else. The folder is named by the Files group,
        // and a long path said twice on one page — once ellipsized to
        // uselessness — was worse than not saying it here at all.
        let counted = match chats.len() {
            0 => "No chats yet".to_string(),
            1 => "1 chat".to_string(),
            many => format!("{many} chats"),
        };
        let caption = gtk::Label::builder().label(counted).xalign(0.0).build();
        caption.add_css_class("dimmed");

        let text = gtk::Box::new(gtk::Orientation::Vertical, 4);
        text.set_hexpand(true);
        text.append(&title);
        text.append(&caption);

        // The one button on the page that opens the dialog. Everything the
        // dialog holds — the name, the tools, the folder — is reachable from
        // here rather than from three different rows.
        let settings = gtk::Button::with_label(if project.is_default() {
            "Chat Settings…"
        } else {
            "Project Settings…"
        });
        settings.set_valign(gtk::Align::Center);
        settings.connect_clicked(clone!(
            #[weak(rename_to = view)]
            self,
            move |_| view.emit_by_name::<()>("edit-project", &[])
        ));

        let line = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        line.append(&text);
        line.append(&settings);
        line.upcast()
    }

    fn instructions(&self, project: &Project) -> gtk::Widget {
        let group = adw::PreferencesGroup::builder()
            .title("Instructions")
            .description(if project.is_default() {
                "Added to what Familiar already knows, in every chat that is not in a project"
            } else {
                "Added to what Familiar already knows, in every chat in this project"
            })
            .build();

        match project.instructions.as_deref().map(str::trim) {
            Some(written) if !written.is_empty() => {
                let label = gtk::Label::builder()
                    .label(written)
                    .xalign(0.0)
                    .wrap(true)
                    .wrap_mode(gtk::pango::WrapMode::WordChar)
                    .selectable(true)
                    .build();
                label.set_margin_top(12);
                label.set_margin_bottom(12);
                label.set_margin_start(12);
                label.set_margin_end(12);

                let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
                card.add_css_class("card");
                card.append(&label);
                group.add(&card);
            }
            _ => {
                // No subtitle: the group's description above already says what
                // instructions are for and where they go, and saying it twice
                // in two sizes reads as two different claims.
                let row = adw::ActionRow::builder().title("Nothing yet").build();
                row.add_css_class("property");
                let list = boxed_list();
                list.append(&row);
                group.add(&list);
            }
        }
        group.upcast()
    }

    fn files_group(&self, project: &Project) -> gtk::Widget {
        let imp = self.imp();
        let group = adw::PreferencesGroup::builder().title("Files").build();

        let folder = project.workspace.as_deref();
        let live = folder.filter(|root| root.is_dir());
        imp.files.set_root(live);

        match (folder, live) {
            (_, Some(root)) => {
                group.set_description(Some(&crate::ui::home_relative(root)));
                group.set_header_suffix(Some(&self.folder_buttons(root)));
                if imp.files.is_empty() {
                    group.add(&note("This folder is empty"));
                } else {
                    let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
                    card.add_css_class("card");
                    card.append(&imp.files);
                    group.add(&card);
                }
            }
            // Chosen once and since moved, renamed or unmounted. Saying so
            // beats an empty box that looks like an empty folder.
            (Some(gone), None) => {
                group.set_header_suffix(Some(&self.choose_button()));
                group.add(&note(&format!(
                    "{} is not there any more",
                    crate::ui::home_relative(gone)
                )));
            }
            (None, None) => {
                group.set_header_suffix(Some(&self.choose_button()));
                group.add(&note(
                    "Choose a folder and its files appear here, for you and — once Files is \
                     switched on — for the assistant",
                ));
            }
        }
        group.upcast()
    }

    fn folder_buttons(&self, root: &Path) -> gtk::Widget {
        let root = root.to_path_buf();
        let open = gtk::Button::from_icon_name("folder-open-symbolic");
        open.set_tooltip_text(Some("Open in Files"));
        open.add_css_class("flat");
        open.connect_clicked(clone!(
            #[weak(rename_to = view)]
            self,
            #[strong]
            root,
            move |_| view.emit_file("reveal", &root)
        ));

        let new_folder = gtk::Button::from_icon_name("folder-new-symbolic");
        new_folder.set_tooltip_text(Some("New Folder"));
        new_folder.add_css_class("flat");
        new_folder.connect_clicked(clone!(
            #[weak(rename_to = view)]
            self,
            #[strong]
            root,
            move |_| view.emit_file("new-folder", &root)
        ));

        let line = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        line.append(&new_folder);
        line.append(&open);
        line.upcast()
    }

    fn choose_button(&self) -> gtk::Widget {
        let choose = gtk::Button::with_label("Choose…");
        choose.set_valign(gtk::Align::Center);
        choose.connect_clicked(clone!(
            #[weak(rename_to = view)]
            self,
            move |_| view.emit_by_name::<()>("choose-folder", &[])
        ));
        choose.upcast()
    }

    fn chats_group(&self, chats: &[ThreadSummary]) -> gtk::Widget {
        let group = adw::PreferencesGroup::builder().title("Chats").build();

        // Search only once there is enough to search. A box over three chats is
        // furniture.
        if chats.len() > 5 {
            let search = gtk::SearchEntry::new();
            search.set_placeholder_text(Some("Search chats"));
            search.set_hexpand(true);
            search.connect_search_changed(clone!(
                #[weak(rename_to = view)]
                self,
                move |entry| {
                    view.imp().query.replace(entry.text().to_string());
                    view.refill_chats();
                }
            ));
            group.set_header_suffix(Some(&search));
        }

        let list = boxed_list();
        self.imp().chat_list.replace(Some(list.clone()));
        group.add(&list);
        self.refill_chats();
        group.upcast()
    }

    /// Put the chats that match the search in the list.
    fn refill_chats(&self) {
        let imp = self.imp();
        let Some(list) = imp.chat_list.borrow().clone() else {
            return;
        };
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }

        let query = imp.query.borrow().trim().to_lowercase();
        let chats = imp.chats.borrow();
        let matching: Vec<&ThreadSummary> = chats
            .iter()
            .filter(|chat| query.is_empty() || chat.title.to_lowercase().contains(&query))
            .collect();

        if matching.is_empty() {
            let row = adw::ActionRow::builder()
                .title(if chats.is_empty() {
                    "No chats yet"
                } else {
                    "Nothing matches"
                })
                .build();
            row.set_sensitive(false);
            list.append(&row);
            return;
        }

        for chat in matching {
            let row = adw::ActionRow::builder()
                .title(&chat.title)
                .subtitle(when(chat))
                .activatable(true)
                .build();
            let id = chat.id.to_string();
            row.connect_activated(clone!(
                #[weak(rename_to = view)]
                self,
                move |_| view.emit_by_name::<()>("chat-chosen", &[&id])
            ));
            list.append(&row);
        }
    }

    fn scheduled_group(&self, scheduled: &[Scheduled]) -> gtk::Widget {
        let group = adw::PreferencesGroup::builder()
            .title("Runs on Its Own")
            .description("Chats here that wake up and ask something for you")
            .build();

        let list = boxed_list();
        for entry in scheduled {
            let row = adw::ActionRow::builder()
                .title(&entry.title)
                .subtitle(format!("{} · {}", entry.schedule, entry.status))
                .activatable(true)
                .build();
            if !entry.enabled {
                row.add_prefix(&gtk::Image::from_icon_name("media-playback-pause-symbolic"));
            }
            let id = entry.thread.clone();
            row.connect_activated(clone!(
                #[weak(rename_to = view)]
                self,
                move |_| view.emit_by_name::<()>("chat-chosen", &[&id])
            ));
            list.append(&row);
        }
        group.add(&list);
        group.upcast()
    }

    fn emit_file(&self, action: &str, path: &Path) {
        self.emit_by_name::<()>(
            "file-action",
            &[&action.to_string(), &path.display().to_string()],
        );
    }

    /// The tree of files it is showing. Its `file.*` actions live on it rather
    /// than here, because they are the tree's.
    pub fn files(&self) -> FileTree {
        self.imp().files.clone()
    }

    /// The files it is showing, for tests.
    pub fn file_rows(&self) -> Vec<crate::ui::file_tree::FileRow> {
        self.imp().files.rows()
    }

    /// The chat rows on show, by title — which is what a search changes.
    pub fn chat_titles(&self) -> Vec<String> {
        let Some(list) = self.imp().chat_list.borrow().clone() else {
            return Vec::new();
        };
        let mut titles = Vec::new();
        let mut child = list.first_child();
        while let Some(row) = child {
            if let Some(row) = row.downcast_ref::<adw::ActionRow>() {
                if row.is_sensitive() {
                    titles.push(row.title().to_string());
                }
            }
            child = row.next_sibling();
        }
        titles
    }

    /// Drive the search box, for tests.
    pub fn search(&self, query: &str) {
        self.imp().query.replace(query.to_string());
        self.refill_chats();
    }
}

fn boxed_list() -> gtk::ListBox {
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.add_css_class("boxed-list");
    list
}

/// A sentence where a list would be, when there is nothing to list.
fn note(text: &str) -> gtk::ListBox {
    let row = adw::ActionRow::builder().title(text).build();
    row.set_sensitive(false);
    let list = boxed_list();
    list.append(&row);
    list
}

/// A chat's subtitle: when it was last touched, and how much is in it.
fn when(chat: &ThreadSummary) -> String {
    let turns = match chat.turns {
        1 => "1 turn".to_string(),
        turns => format!("{turns} turns"),
    };
    format!("{} · {turns}", crate::ui::sidebar::relative(chat.updated))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chat_subtitle_says_when_and_how_much() {
        let chat = ThreadSummary {
            id: crate::model::thread::ThreadId::from_stem("t").expect("id"),
            title: "x".into(),
            updated: chrono::Utc::now(),
            turns: 3,
        };
        assert!(when(&chat).ends_with("· 3 turns"));
    }
}
