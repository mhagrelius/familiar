//! Projects and their chats.
//!
//! A tree, because a project *contains* chats and a flat list cannot say so.
//! Every root is a project — the first one is the chats that belong to no
//! project — and opening one shows a way to start a chat and the chats
//! themselves.
//!
//! `GtkListView` over a `GtkTreeListModel`, which is the only combination in
//! GTK that does collapsing rows. The alternative, `AdwSidebar`, is flat.
//!
//! **The files are not here.** They were, for a day: a folder tree nested
//! inside 300 pixels, competing with the chats, and an empty folder read as a
//! dead end rather than as an empty folder. They live on the project's own page
//! now — see [`crate::ui::ProjectView`] — which is what clicking a project row
//! opens.
//!
//! **A rebuild keeps what was open.** The application refreshes this after
//! every turn, and a tree that collapsed each time would be unusable, so the
//! expanded rows are remembered by key and reopened.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::sync::OnceLock;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib::subclass::Signal;
use gtk::glib::{self, clone};

use crate::model::project::{Project, ThreadSummary};
use crate::model::thread::ThreadId;

/// What one row stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// A project. The default one is a project too; it just has no name to
    /// change and cannot be deleted.
    Project {
        slug: String,
        default: bool,
    },
    NewChat {
        slug: String,
    },
    Chat {
        slug: String,
        id: ThreadId,
    },
}

impl Row {
    /// Which project this row belongs to.
    pub fn slug(&self) -> &str {
        match self {
            Self::Project { slug, .. } | Self::NewChat { slug } | Self::Chat { slug, .. } => slug,
        }
    }

    /// A name that survives a rebuild, so a row that was open can be reopened,
    /// the one that was selected can be found again, and a menu item can say
    /// which row it acts on.
    pub fn key(&self) -> String {
        match self {
            Self::Project { slug, .. } => format!("p:{slug}"),
            Self::NewChat { slug } => format!("n:{slug}"),
            Self::Chat { slug, id } => format!("c:{slug}:{id}"),
        }
    }

    /// The menu this row offers on a right-click, or nothing.
    ///
    /// Every item carries the row's key as its action target rather than the
    /// widget remembering which row was clicked. A menu that acts on remembered
    /// state acts on the wrong row the moment anything reorders underneath it —
    /// and this way `row.rename-chat` with a key does the same thing from a
    /// pointer, a keyboard, or a test.
    fn menu(&self) -> Option<gio::Menu> {
        let key = self.key();
        let item = |label: &str, action: &str| {
            let item = gio::MenuItem::new(Some(label), None);
            item.set_action_and_target_value(Some(action), Some(&key.to_variant()));
            item
        };

        let menu = gio::Menu::new();
        match self {
            Self::Project { default, .. } => {
                menu.append_item(&item("New Chat", "row.new-chat"));
                let settings = gio::Menu::new();
                settings.append_item(&item(
                    if *default {
                        "Chat Settings…"
                    } else {
                        "Project Settings…"
                    },
                    "row.edit-project",
                ));
                menu.append_section(None, &settings);
                if !default {
                    let danger = gio::Menu::new();
                    danger.append_item(&item("Delete Project…", "row.delete-project"));
                    menu.append_section(None, &danger);
                }
            }
            Self::Chat { .. } => {
                menu.append_item(&item("Rename…", "row.rename-chat"));
                menu.append_item(&item("Delete", "row.delete-chat"));
            }
            Self::NewChat { .. } => return None,
        }
        Some(menu)
    }
}

/// One row's contents. A `GObject` because that is what a list model holds.
mod node {
    use super::*;

    #[derive(Default)]
    pub struct Node {
        pub row: RefCell<Option<Row>>,
        pub title: RefCell<String>,
        pub subtitle: RefCell<String>,
        pub icon: RefCell<Option<gio::Icon>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Node {
        const NAME: &'static str = "FamiliarSidebarNode";
        type Type = super::Node;
    }

    impl ObjectImpl for Node {}
}

glib::wrapper! {
    pub struct Node(ObjectSubclass<node::Node>);
}

impl Node {
    fn new(row: Row, title: &str, subtitle: &str, icon: Option<gio::Icon>) -> Self {
        let node: Self = glib::Object::new();
        node.imp().row.replace(Some(row));
        node.imp().title.replace(title.to_string());
        node.imp().subtitle.replace(subtitle.to_string());
        node.imp().icon.replace(icon);
        node
    }

    pub fn row(&self) -> Row {
        self.imp()
            .row
            .borrow()
            .clone()
            .expect("every node has a row")
    }
}

fn themed(name: &str) -> Option<gio::Icon> {
    Some(gio::ThemedIcon::new(name).upcast())
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Sidebar {
        pub list: RefCell<Option<gtk::ListView>>,
        pub selection: RefCell<Option<gtk::SingleSelection>>,
        pub roots: RefCell<Option<gio::ListStore>>,
        pub tree: RefCell<Option<gtk::TreeListModel>>,
        pub menu: RefCell<Option<gtk::PopoverMenu>>,
        pub actions: RefCell<Option<gio::SimpleActionGroup>>,
        /// What the projects and their chats are, for the tree to build
        /// children out of when a row is expanded.
        pub projects: RefCell<Vec<(Project, Vec<ThreadSummary>)>>,
        /// The rows that were open, by key, so a rebuild does not collapse the
        /// tree under whoever was reading it.
        pub expanded: RefCell<HashSet<String>>,
        /// Set while the tree is being rebuilt, so an expansion the code is
        /// restoring is not recorded as one the user asked for.
        pub restoring: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Sidebar {
        const NAME: &'static str = "FamiliarSidebar";
        type Type = super::Sidebar;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_layout_manager_type::<gtk::BinLayout>();
        }
    }

    impl ObjectImpl for Sidebar {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().build();
        }

        fn dispose(&self) {
            // The popover is parented to this widget rather than packed into
            // it, so it does not go away with the children.
            if let Some(menu) = self.menu.borrow_mut().take() {
                menu.unparent();
            }
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }

        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![
                    // project slug, chat id
                    Signal::builder("thread-chosen")
                        .param_types([String::static_type(), String::static_type()])
                        .build(),
                    // project slug
                    Signal::builder("new-thread")
                        .param_types([String::static_type()])
                        .build(),
                    Signal::builder("thread-rename")
                        .param_types([String::static_type(), String::static_type()])
                        .build(),
                    Signal::builder("thread-delete")
                        .param_types([String::static_type(), String::static_type()])
                        .build(),
                    // what to do, and to which project: "open", "edit",
                    // "delete". One signal rather than three, because the
                    // window's only job with any of them is to pass it on.
                    Signal::builder("project-action")
                        .param_types([String::static_type(), String::static_type()])
                        .build(),
                ]
            })
        }
    }

    impl WidgetImpl for Sidebar {}
}

glib::wrapper! {
    pub struct Sidebar(ObjectSubclass<imp::Sidebar>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for Sidebar {
    fn default() -> Self {
        Self::new()
    }
}

impl Sidebar {
    pub fn new() -> Self {
        glib::Object::new()
    }

    fn build(&self) {
        let roots = gio::ListStore::new::<Node>();
        let tree = gtk::TreeListModel::new(
            roots.clone(),
            false,
            false,
            clone!(
                #[weak(rename_to = widget)]
                self,
                #[upgrade_or]
                None,
                move |item: &glib::Object| widget.children_of(item)
            ),
        );
        let selection = gtk::SingleSelection::new(Some(tree.clone()));
        selection.set_can_unselect(true);
        selection.set_autoselect(false);

        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let icon = gtk::Image::new();
            let title = gtk::Label::builder()
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build();
            let subtitle = gtk::Label::builder()
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build();
            subtitle.add_css_class("caption");
            subtitle.add_css_class("dimmed");

            let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
            text.set_valign(gtk::Align::Center);
            text.set_hexpand(true);
            text.append(&title);
            text.append(&subtitle);

            let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            line.append(&icon);
            line.append(&text);

            let expander = gtk::TreeExpander::new();
            expander.set_child(Some(&line));
            // `.navigation-sidebar > row` is `padding: 0` on purpose — the
            // class expects the row's own content to carry its inset, which is
            // what AdwSidebar's rows do and what these did not: a long chat
            // title ran into the right edge of its own highlight.
            expander.set_margin_start(6);
            expander.set_margin_end(10);
            expander.set_margin_top(5);
            expander.set_margin_bottom(5);
            item.set_child(Some(&expander));
        });
        factory.connect_bind(clone!(
            #[weak(rename_to = widget)]
            self,
            move |_, item| widget.bind_row(item)
        ));
        factory.connect_unbind(|_, item| {
            // The right-click gesture carries the row it was bound to, so it
            // goes away with the binding rather than following a recycled row
            // onto a different one.
            let Some(expander) = item
                .downcast_ref::<gtk::ListItem>()
                .and_then(|item| item.child())
                .and_downcast::<gtk::TreeExpander>()
            else {
                return;
            };
            let Some(line) = expander.child() else { return };
            let controllers = line.observe_controllers();
            for index in (0..controllers.n_items()).rev() {
                let Some(controller) = controllers
                    .item(index)
                    .and_downcast::<gtk::EventController>()
                else {
                    continue;
                };
                if controller.name().as_deref() == Some("row-menu") {
                    line.remove_controller(&controller);
                }
            }
        });

        let list = gtk::ListView::new(Some(selection.clone()), Some(factory));
        list.add_css_class("navigation-sidebar");
        // A sidebar opens what you click, once. Two clicks is what a file
        // manager wants and not what a list of conversations wants.
        list.set_single_click_activate(true);
        list.connect_activate(clone!(
            #[weak(rename_to = widget)]
            self,
            move |_, position| widget.activated(position)
        ));

        let actions = gio::SimpleActionGroup::new();
        for name in [
            "new-chat",
            "edit-project",
            "choose-folder",
            "delete-project",
            "rename-chat",
            "delete-chat",
            "open-file",
            "reveal",
            "new-folder",
            "rename-file",
            "trash",
        ] {
            let action = gio::SimpleAction::new(name, Some(glib::VariantTy::STRING));
            action.connect_activate(clone!(
                #[weak(rename_to = widget)]
                self,
                move |_, key| {
                    if let Some(key) = key.and_then(|key| key.str()) {
                        widget.menu_action(name, key);
                    }
                }
            ));
            actions.add_action(&action);
        }
        self.insert_action_group("row", Some(&actions));

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&list)
            .build();
        scroller.set_parent(self);
        self.set_vexpand(true);

        let imp = self.imp();
        imp.roots.replace(Some(roots));
        imp.tree.replace(Some(tree));
        imp.selection.replace(Some(selection));
        imp.list.replace(Some(list));
        // No popover is built here: one is built per menu, in `show_menu`.
        imp.actions.replace(Some(actions));
    }

    // -- the tree --------------------------------------------------------------

    /// What opens under a row. `None` means the row does not open at all, which
    /// is what makes a chat a leaf and a project a branch.
    fn children_of(&self, item: &glib::Object) -> Option<gio::ListModel> {
        let node = item.downcast_ref::<Node>()?;
        match node.row() {
            Row::Project { slug, .. } => {
                let store = gio::ListStore::new::<Node>();
                let projects = self.imp().projects.borrow();
                let (_, threads) = projects.iter().find(|(project, _)| project.slug == slug)?;

                store.append(&Node::new(
                    Row::NewChat { slug: slug.clone() },
                    "New Chat",
                    "",
                    themed("list-add-symbolic"),
                ));
                for thread in threads {
                    store.append(&Node::new(
                        Row::Chat {
                            slug: slug.clone(),
                            id: thread.id.clone(),
                        },
                        &thread.title,
                        &when(thread),
                        None,
                    ));
                }
                Some(store.upcast())
            }
            Row::NewChat { .. } | Row::Chat { .. } => None,
        }
    }

    fn bind_row(&self, item: &glib::Object) {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(list_row) = item.item().and_downcast::<gtk::TreeListRow>() else {
            return;
        };
        let Some(node) = list_row.item().and_downcast::<Node>() else {
            return;
        };
        let Some(expander) = item.child().and_downcast::<gtk::TreeExpander>() else {
            return;
        };
        expander.set_list_row(Some(&list_row));

        let Some(line) = expander.child().and_downcast::<gtk::Box>() else {
            return;
        };
        let Some(icon) = line.first_child().and_downcast::<gtk::Image>() else {
            return;
        };
        let Some(text) = icon.next_sibling().and_downcast::<gtk::Box>() else {
            return;
        };
        let (Some(title), Some(subtitle)) = (
            text.first_child().and_downcast::<gtk::Label>(),
            text.last_child().and_downcast::<gtk::Label>(),
        ) else {
            return;
        };

        let inner = node.imp();
        match inner.icon.borrow().as_ref() {
            Some(image) => {
                icon.set_from_gicon(image);
                icon.set_visible(true);
            }
            None => icon.set_visible(false),
        }
        title.set_label(&inner.title.borrow());
        let caption = inner.subtitle.borrow().clone();
        subtitle.set_label(&caption);
        subtitle.set_visible(!caption.is_empty());
        // "New Chat" does something rather than being somewhere, so it is
        // activatable but never the selected row — leaving the highlight on it
        // would say the open conversation is a button.
        item.set_selectable(!matches!(node.row(), Row::NewChat { .. }));

        let row = node.row();
        title.set_tooltip_text(Some(&inner.title.borrow()));

        if row.menu().is_some() {
            let gesture = gtk::GestureClick::new();
            gesture.set_name(Some("row-menu"));
            gesture.set_button(gtk::gdk::BUTTON_SECONDARY);
            gesture.connect_pressed(clone!(
                #[weak(rename_to = widget)]
                self,
                #[weak]
                line,
                move |_, _, x, y| widget.show_menu(&row, line.upcast_ref(), x, y)
            ));
            line.add_controller(gesture);
        }
    }

    fn activated(&self, position: u32) {
        let Some(row) = self.row_at(position) else {
            return;
        };
        match row {
            Row::Chat { slug, id } => {
                self.emit_by_name::<()>("thread-chosen", &[&slug, &id.to_string()]);
            }
            Row::NewChat { slug } => self.emit_by_name::<()>("new-thread", &[&slug]),
            // Clicking a project opens the project, which is its page — its
            // files, its instructions, what it has running. Expanding it to see
            // the chats is the arrow's job, and `GtkTreeExpander` gives that
            // away for free: the arrow is a button and swallows its own click.
            Row::Project { slug, .. } => self.emit_project("open", &slug),
        }
    }

    /// Note which rows are open, from the tree itself.
    ///
    /// Read back rather than recorded as it happens: the disclosure arrow is a
    /// button inside `GtkTreeExpander` and expands the row without telling
    /// anyone, so a set updated only where this widget handles a click would
    /// quietly fall out of step and collapse the tree on the next refresh.
    fn remember_expansion(&self) {
        let Some(tree) = self.imp().tree.borrow().clone() else {
            return;
        };
        let open: HashSet<String> = (0..tree.n_items())
            .filter_map(|position| {
                let list_row = tree.item(position).and_downcast::<gtk::TreeListRow>()?;
                list_row.is_expanded().then(|| {
                    list_row
                        .item()
                        .and_downcast::<Node>()
                        .map(|node| node.row().key())
                })?
            })
            .collect();
        if !open.is_empty() {
            self.imp().expanded.replace(open);
        }
    }

    fn list_row_at(&self, position: u32) -> Option<gtk::TreeListRow> {
        self.imp()
            .selection
            .borrow()
            .as_ref()?
            .item(position)
            .and_downcast::<gtk::TreeListRow>()
    }

    fn row_at(&self, position: u32) -> Option<Row> {
        self.list_row_at(position)?
            .item()
            .and_downcast::<Node>()
            .map(|node| node.row())
    }

    // -- menus -----------------------------------------------------------------

    fn show_menu(&self, row: &Row, at: &gtk::Widget, x: f64, y: f64) {
        let Some(model) = row.menu() else { return };
        // A popover built for *this* menu, rather than one popover whose model
        // is swapped. `GtkPopoverMenu::set_menu_model` rebuilds the popover's
        // contents, and popping it up in the same frame shows one that has not
        // been built yet — so nothing appears. Right-click the same row again
        // and the model is unchanged, nothing is rebuilt, and it works: which
        // is exactly the "I have to click twice on a different row" this was.
        //
        // The previous one is taken down and unparented first. A popover left
        // parented is a warning at dispose and a leak before it.
        if let Some(previous) = self.imp().menu.borrow_mut().take() {
            previous.popdown();
            previous.unparent();
        }
        let menu = gtk::PopoverMenu::from_model(Some(&model));
        menu.set_has_arrow(false);
        menu.set_halign(gtk::Align::Start);
        menu.set_parent(self);

        // The pointer is in the row's coordinates and the popover is parented
        // to the sidebar, so the point has to be translated or every menu opens
        // at the top of the list.
        let point = at
            .compute_point(self, &gtk::graphene::Point::new(x as f32, y as f32))
            .unwrap_or_else(|| gtk::graphene::Point::new(x as f32, y as f32));
        menu.set_pointing_to(Some(&gtk::gdk::Rectangle::new(
            point.x() as i32,
            point.y() as i32,
            1,
            1,
        )));
        menu.popup();
        self.imp().menu.replace(Some(menu));
    }

    /// Carry out one menu item on the row its target names.
    ///
    /// A key that names no row on screen does nothing: the tree is rebuilt
    /// after every turn, and a menu left open across one could otherwise act on
    /// a chat that has since been deleted.
    fn menu_action(&self, name: &str, key: &str) {
        let Some(row) = self.rows().into_iter().find(|row| row.key() == key) else {
            return;
        };
        let slug = row.slug().to_string();
        match (name, &row) {
            ("new-chat", _) => self.emit_by_name::<()>("new-thread", &[&slug]),
            ("edit-project", _) => self.emit_project("edit", &slug),
            ("delete-project", _) => self.emit_project("delete", &slug),
            ("rename-chat", Row::Chat { id, .. }) => {
                self.emit_by_name::<()>("thread-rename", &[&slug, &id.to_string()]);
            }
            ("delete-chat", Row::Chat { id, .. }) => {
                self.emit_by_name::<()>("thread-delete", &[&slug, &id.to_string()]);
            }
            _ => {}
        }
    }

    fn emit_project(&self, action: &str, slug: &str) {
        self.emit_by_name::<()>("project-action", &[&action.to_string(), &slug.to_string()]);
    }

    // -- what the application tells it -----------------------------------------

    /// Rebuild the tree. A person has a handful of projects and hundreds of
    /// chats at most, and a diff that is subtly wrong shows a conversation that
    /// is not there.
    pub fn set_projects(&self, projects: &[(Project, Vec<ThreadSummary>)]) {
        let imp = self.imp();
        let (Some(roots), Some(tree)) = (imp.roots.borrow().clone(), imp.tree.borrow().clone())
        else {
            return;
        };

        self.remember_expansion();

        // A project that has gone stops being remembered as open, or the set
        // grows for the life of the application.
        let live: HashSet<String> = projects
            .iter()
            .map(|(project, _)| format!("p:{}", project.slug))
            .collect();
        imp.expanded.borrow_mut().retain(|key| live.contains(key));

        imp.projects.replace(projects.to_vec());
        imp.restoring.set(true);
        roots.remove_all();
        for (project, _) in projects {
            roots.append(&Node::new(
                Row::Project {
                    slug: project.slug.clone(),
                    default: project.is_default(),
                },
                &project.name,
                "",
                themed(if project.is_default() {
                    "user-home-symbolic"
                } else {
                    "folder-symbolic"
                }),
            ));
        }
        self.restore_expansion(&tree);
        imp.restoring.set(false);
    }

    /// Reopen what was open. Expanding a row adds rows below it, so this goes
    /// round again until a pass changes nothing — which is at most as many
    /// passes as the tree is deep.
    fn restore_expansion(&self, tree: &gtk::TreeListModel) {
        let wanted = self.imp().expanded.borrow().clone();
        if wanted.is_empty() {
            return;
        }
        for _ in 0..8 {
            let mut opened = false;
            for position in 0..tree.n_items() {
                let Some(list_row) = tree.item(position).and_downcast::<gtk::TreeListRow>() else {
                    continue;
                };
                if list_row.is_expanded() || !list_row.is_expandable() {
                    continue;
                }
                let Some(node) = list_row.item().and_downcast::<Node>() else {
                    continue;
                };
                if wanted.contains(&node.row().key()) {
                    list_row.set_expanded(true);
                    opened = true;
                }
            }
            if !opened {
                return;
            }
        }
    }

    /// Every row the tree is showing, in order — what is under a collapsed
    /// project is not here, because it is not on screen either.
    pub fn rows(&self) -> Vec<Row> {
        let Some(tree) = self.imp().tree.borrow().clone() else {
            return Vec::new();
        };
        (0..tree.n_items())
            .filter_map(|position| {
                tree.item(position)
                    .and_downcast::<gtk::TreeListRow>()?
                    .item()
                    .and_downcast::<Node>()
                    .map(|node| node.row())
            })
            .collect()
    }

    /// The row the highlight is on, if it is on one.
    pub fn selected(&self) -> Option<Row> {
        let selection = self.imp().selection.borrow().clone()?;
        let position = selection.selected();
        (position != gtk::INVALID_LIST_POSITION).then_some(())?;
        self.row_at(position)
    }

    /// Open a project's chats, without going through it being clicked.
    pub fn open_project(&self, slug: &str) {
        let imp = self.imp();
        imp.expanded.borrow_mut().insert(format!("p:{slug}"));
        if let Some(tree) = imp.tree.borrow().clone() {
            self.restore_expansion(&tree);
        }
    }

    /// Highlight a chat without reporting it as chosen, opening whatever has to
    /// open to make it visible.
    pub fn select(&self, slug: &str, id: Option<&ThreadId>) {
        let imp = self.imp();
        let (Some(selection), Some(tree)) =
            (imp.selection.borrow().clone(), imp.tree.borrow().clone())
        else {
            return;
        };

        // With no chat open the project is still where you are, so its own row
        // is what gets the highlight.
        let wanted = match id {
            Some(id) => Row::Chat {
                slug: slug.to_string(),
                id: id.clone(),
            },
            None => Row::Project {
                slug: slug.to_string(),
                default: false,
            },
        };
        let key = wanted.key();

        // A chat inside a collapsed project cannot be selected, and being taken
        // to a conversation the sidebar does not show would be worse than not
        // showing the selection.
        if id.is_some() {
            imp.expanded.borrow_mut().insert(format!("p:{slug}"));
            self.restore_expansion(&tree);
        }

        let found = (0..selection.n_items()).find(|position| {
            selection
                .item(*position)
                .and_downcast::<gtk::TreeListRow>()
                .and_then(|list_row| list_row.item().and_downcast::<Node>())
                .is_some_and(|node| node.row().key() == key)
        });
        match found {
            Some(position) => selection.set_selected(position),
            None => selection.set_selected(gtk::INVALID_LIST_POSITION),
        }
    }
}

/// A chat's subtitle: when it was last touched, and how much is in it.
fn when(thread: &ThreadSummary) -> String {
    let turns = match thread.turns {
        1 => "1 turn".to_string(),
        turns => format!("{turns} turns"),
    };
    format!("{} · {turns}", relative(thread.updated))
}

/// A timestamp as a person would say it. Relative near the present, because
/// "yesterday" is what you remember, and dated once it is far enough back that
/// counting days stops being useful.
///
/// Public because the project page lists the same chats and has to say when in
/// the same words — two spellings of "yesterday" in one window would be a bug
/// nobody could name.
pub fn relative(updated: chrono::DateTime<chrono::Utc>) -> String {
    use chrono::{Datelike, Local, TimeZone};

    let updated = Local.from_utc_datetime(&updated.naive_utc());
    let today = Local::now().date_naive();
    let days = (today - updated.date_naive()).num_days();
    match days {
        ..=-1 => updated.format("%-d %b").to_string(),
        0 => updated.format("%H:%M").to_string(),
        1 => "Yesterday".to_string(),
        2..=6 => updated.format("%A").to_string(),
        _ if updated.year() == today.year() => updated.format("%-d %b").to_string(),
        _ => updated.format("%-d %b %Y").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};

    #[test]
    fn today_is_a_time_and_yesterday_is_a_word() {
        assert_eq!(relative(Utc::now() - Duration::days(1)), "Yesterday");
        assert!(relative(Utc::now()).contains(':'));
    }

    #[test]
    fn last_month_is_a_date_not_a_day_name() {
        let long_ago = relative(Utc::now() - Duration::days(40));
        assert!(!long_ago.contains(':'), "{long_ago}");
        assert!(long_ago.chars().any(|c| c.is_ascii_digit()), "{long_ago}");
    }

    #[test]
    fn a_subtitle_counts_the_turns() {
        let summary = |turns| ThreadSummary {
            id: ThreadId::from_stem("t").expect("id"),
            title: "x".into(),
            updated: Utc::now(),
            turns,
        };
        assert!(when(&summary(1)).ends_with("· 1 turn"));
        assert!(when(&summary(9)).ends_with("· 9 turns"));
    }

    #[test]
    fn a_key_tells_two_rows_of_the_same_kind_apart() {
        let chat = |slug: &str, id: &str| Row::Chat {
            slug: slug.into(),
            id: ThreadId::from_stem(id).expect("id"),
        };
        assert_ne!(chat("default", "a").key(), chat("default", "b").key());
        assert_ne!(chat("default", "a").key(), chat("planning", "a").key());
        assert_ne!(
            Row::Project {
                slug: "default".into(),
                default: true
            }
            .key(),
            Row::NewChat {
                slug: "default".into()
            }
            .key()
        );
    }

    /// The default project's row is found by slug, and whether the caller
    /// happened to know it was the default one must not change that — `select`
    /// builds the key without knowing.
    #[test]
    fn a_project_key_does_not_depend_on_it_being_the_default_one() {
        let default = Row::Project {
            slug: "default".into(),
            default: true,
        };
        let not = Row::Project {
            slug: "default".into(),
            default: false,
        };
        assert_eq!(default.key(), not.key());
    }

    #[test]
    fn only_the_rows_that_can_be_acted_on_have_a_menu() {
        assert!(Row::NewChat {
            slug: "default".into()
        }
        .menu()
        .is_none());
        assert!(Row::Chat {
            slug: "default".into(),
            id: ThreadId::from_stem("t").expect("id")
        }
        .menu()
        .is_some());
    }

    /// The default project has no name to change and nothing to delete, so its
    /// menu says neither.
    #[test]
    fn the_default_project_offers_no_way_to_delete_itself() {
        let default = Row::Project {
            slug: "default".into(),
            default: true,
        }
        .menu()
        .expect("a menu");
        let named = Row::Project {
            slug: "planning".into(),
            default: false,
        }
        .menu()
        .expect("a menu");
        assert!(!menu_mentions(&default, "row.delete-project"));
        assert!(menu_mentions(&named, "row.delete-project"));
    }

    /// Whether any item anywhere in the menu points at an action.
    fn menu_mentions(menu: &gio::Menu, action: &str) -> bool {
        use gtk::prelude::*;

        let model: &gio::MenuModel = menu.upcast_ref();
        for index in 0..model.n_items() {
            if model
                .item_attribute_value(index, gio::MENU_ATTRIBUTE_ACTION, None)
                .and_then(|value| value.get::<String>())
                .is_some_and(|named| named == action)
            {
                return true;
            }
            for link in [gio::MENU_LINK_SECTION, gio::MENU_LINK_SUBMENU] {
                if let Some(section) = model.item_link(index, link) {
                    if let Ok(section) = section.downcast::<gio::Menu>() {
                        if menu_mentions(&section, action) {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    #[test]
    fn a_folder_under_home_is_shown_the_short_way() {
        let home = glib::home_dir();
        assert_eq!(crate::ui::home_relative(&home.join("Notes")), "~/Notes");
        assert_eq!(
            crate::ui::home_relative(std::path::Path::new("/srv/data")),
            "/srv/data"
        );
    }
}
