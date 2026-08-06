//! The files under a folder, as a tree that opens.
//!
//! This lived in the sidebar until 2026-08-03, where it never had the width to
//! be read: a path, an icon and a disclosure arrow inside 300 pixels, with the
//! chats it was competing with. It belongs on the project's own page, which is
//! as wide as the window.
//!
//! It **reads** the filesystem and changes nothing. Listing a directory is what
//! drawing a tree *is*; making, renaming and trashing leave as a `file-action`
//! signal for [`crate::ui::Application`], which is where the check that a path
//! is still inside the project lives.

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib::subclass::Signal;
use gtk::glib::{self, clone};

/// How many entries of one directory are drawn. A folder with ten thousand
/// files in it is not a thing to scroll, and reading it would stall the frame
/// that expanded it.
const MAX_ENTRIES: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileRow {
    Folder(PathBuf),
    File(PathBuf),
}

impl FileRow {
    pub fn path(&self) -> &Path {
        match self {
            Self::Folder(path) | Self::File(path) => path,
        }
    }

    /// A name that survives a rebuild, so a folder that was open can be
    /// reopened and a menu item can say which row it acts on.
    pub fn key(&self) -> String {
        match self {
            Self::Folder(path) => format!("d:{}", path.display()),
            Self::File(path) => format!("x:{}", path.display()),
        }
    }

    fn menu(&self) -> gio::Menu {
        let key = self.key();
        let item = |label: &str, action: &str| {
            let item = gio::MenuItem::new(Some(label), None);
            item.set_action_and_target_value(Some(action), Some(&key.to_variant()));
            item
        };

        let menu = gio::Menu::new();
        match self {
            Self::Folder(_) => {
                menu.append_item(&item("Open in Files", "file.reveal"));
                menu.append_item(&item("New Folder…", "file.new-folder"));
            }
            Self::File(_) => {
                menu.append_item(&item("Open", "file.open"));
                menu.append_item(&item("Show in Files", "file.reveal"));
            }
        }
        let edit = gio::Menu::new();
        edit.append_item(&item("Rename…", "file.rename"));
        edit.append_item(&item("Move to Trash", "file.trash"));
        menu.append_section(None, &edit);
        menu
    }
}

mod node {
    use super::*;

    #[derive(Default)]
    pub struct Node {
        pub row: RefCell<Option<FileRow>>,
        pub name: RefCell<String>,
        pub icon: RefCell<Option<gio::Icon>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Node {
        const NAME: &'static str = "FamiliarFileNode";
        type Type = super::Node;
    }

    impl ObjectImpl for Node {}
}

glib::wrapper! {
    pub struct Node(ObjectSubclass<node::Node>);
}

impl Node {
    fn new(row: FileRow, name: &str, icon: gio::Icon) -> Self {
        let node: Self = glib::Object::new();
        node.imp().row.replace(Some(row));
        node.imp().name.replace(name.to_string());
        node.imp().icon.replace(Some(icon));
        node
    }

    fn row(&self) -> FileRow {
        self.imp()
            .row
            .borrow()
            .clone()
            .expect("every node has a row")
    }
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct FileTree {
        pub list: RefCell<Option<gtk::ListView>>,
        pub selection: RefCell<Option<gtk::SingleSelection>>,
        pub roots: RefCell<Option<gio::ListStore>>,
        pub tree: RefCell<Option<gtk::TreeListModel>>,
        pub menu: RefCell<Option<gtk::PopoverMenu>>,
        pub root: RefCell<Option<PathBuf>>,
        pub expanded: RefCell<HashSet<String>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FileTree {
        const NAME: &'static str = "FamiliarFileTree";
        type Type = super::FileTree;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_layout_manager_type::<gtk::BinLayout>();
        }
    }

    impl ObjectImpl for FileTree {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().build();
        }

        fn dispose(&self) {
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
                    // what to do, and to which path: "open", "reveal",
                    // "new-folder", "rename", "trash"
                    Signal::builder("file-action")
                        .param_types([String::static_type(), String::static_type()])
                        .build(),
                ]
            })
        }
    }

    impl WidgetImpl for FileTree {}
}

glib::wrapper! {
    pub struct FileTree(ObjectSubclass<imp::FileTree>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for FileTree {
    fn default() -> Self {
        Self::new()
    }
}

impl FileTree {
    pub fn new() -> Self {
        glib::Object::new()
    }

    fn build(&self) {
        let roots = gio::ListStore::new::<Node>();
        let tree = gtk::TreeListModel::new(roots.clone(), false, false, |item: &glib::Object| {
            let node = item.downcast_ref::<Node>()?;
            match node.row() {
                FileRow::Folder(path) => {
                    let store = gio::ListStore::new::<Node>();
                    for node in entries_of(&path) {
                        store.append(&node);
                    }
                    Some(store.upcast())
                }
                FileRow::File(_) => None,
            }
        });
        let selection = gtk::SingleSelection::new(Some(tree.clone()));
        selection.set_can_unselect(true);
        selection.set_autoselect(false);

        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let icon = gtk::Image::new();
            let name = gtk::Label::builder()
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .hexpand(true)
                .build();

            let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            line.append(&icon);
            line.append(&name);

            let expander = gtk::TreeExpander::new();
            expander.set_child(Some(&line));
            expander.set_margin_start(6);
            expander.set_margin_end(10);
            expander.set_margin_top(4);
            expander.set_margin_bottom(4);
            item.set_child(Some(&expander));
        });
        factory.connect_bind(clone!(
            #[weak(rename_to = widget)]
            self,
            move |_, item| widget.bind_row(item)
        ));
        factory.connect_unbind(|_, item| {
            let Some(line) = item
                .downcast_ref::<gtk::ListItem>()
                .and_then(|item| item.child())
                .and_downcast::<gtk::TreeExpander>()
                .and_then(|expander| expander.child())
            else {
                return;
            };
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
        list.set_single_click_activate(true);
        list.connect_activate(clone!(
            #[weak(rename_to = widget)]
            self,
            move |_, position| widget.activated(position)
        ));

        let actions = gio::SimpleActionGroup::new();
        for name in ["open", "reveal", "new-folder", "rename", "trash"] {
            let action = gio::SimpleAction::new(name, Some(glib::VariantTy::STRING));
            action.connect_activate(clone!(
                #[weak(rename_to = widget)]
                self,
                move |_, key| {
                    if let Some(row) = key
                        .and_then(|key| key.str().map(str::to_string))
                        .and_then(|key| widget.rows().into_iter().find(|row| row.key() == key))
                    {
                        widget.emit_file(name, row.path());
                    }
                }
            ));
            actions.add_action(&action);
        }
        self.insert_action_group("file", Some(&actions));

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(420)
            .child(&list)
            .build();
        scroller.set_parent(self);

        let imp = self.imp();
        imp.roots.replace(Some(roots));
        imp.tree.replace(Some(tree));
        imp.selection.replace(Some(selection));
        imp.list.replace(Some(list));
        // No popover is built here: one is built per menu, in `show_menu`.
    }

    fn bind_row(&self, item: &glib::Object) {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let (Some(list_row), Some(expander)) = (
            item.item().and_downcast::<gtk::TreeListRow>(),
            item.child().and_downcast::<gtk::TreeExpander>(),
        ) else {
            return;
        };
        let Some(node) = list_row.item().and_downcast::<Node>() else {
            return;
        };
        expander.set_list_row(Some(&list_row));

        let Some(line) = expander.child().and_downcast::<gtk::Box>() else {
            return;
        };
        let Some(icon) = line.first_child().and_downcast::<gtk::Image>() else {
            return;
        };
        let Some(name) = icon.next_sibling().and_downcast::<gtk::Label>() else {
            return;
        };

        if let Some(image) = node.imp().icon.borrow().as_ref() {
            icon.set_from_gicon(image);
        }
        let label = node.imp().name.borrow().clone();
        name.set_label(&label);
        name.set_tooltip_text(Some(&label));

        let row = node.row();
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

    fn activated(&self, position: u32) {
        let Some(list_row) = self
            .imp()
            .selection
            .borrow()
            .as_ref()
            .and_then(|selection| selection.item(position))
            .and_downcast::<gtk::TreeListRow>()
        else {
            return;
        };
        let Some(node) = list_row.item().and_downcast::<Node>() else {
            return;
        };
        match node.row() {
            FileRow::File(path) => self.emit_file("open", &path),
            FileRow::Folder(path) => {
                let opening = !list_row.is_expanded();
                list_row.set_expanded(opening);
                let key = FileRow::Folder(path).key();
                let mut expanded = self.imp().expanded.borrow_mut();
                if opening {
                    expanded.insert(key);
                } else {
                    expanded.remove(&key);
                }
            }
        }
    }

    fn show_menu(&self, row: &FileRow, at: &gtk::Widget, x: f64, y: f64) {
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
        let menu = gtk::PopoverMenu::from_model(Some(&row.menu()));
        menu.set_has_arrow(false);
        menu.set_halign(gtk::Align::Start);
        menu.set_parent(self);
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

    fn emit_file(&self, action: &str, path: &Path) {
        self.emit_by_name::<()>(
            "file-action",
            &[&action.to_string(), &path.display().to_string()],
        );
    }

    /// Note which folders are open, from the tree itself.
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

    /// Show what is under `root`, keeping open whatever was open before.
    pub fn set_root(&self, root: Option<&Path>) {
        let imp = self.imp();
        let Some(roots) = imp.roots.borrow().clone() else {
            return;
        };
        let changed = imp.root.borrow().as_deref() != root;
        if changed {
            // A different folder's open rows mean nothing here.
            imp.expanded.borrow_mut().clear();
        } else {
            self.remember_expansion();
        }
        imp.root.replace(root.map(Path::to_path_buf));

        roots.remove_all();
        if let Some(root) = root {
            for node in entries_of(root) {
                roots.append(&node);
            }
        }
        self.restore_expansion();
    }

    fn restore_expansion(&self) {
        let wanted = self.imp().expanded.borrow().clone();
        let Some(tree) = self.imp().tree.borrow().clone() else {
            return;
        };
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

    /// Every row on show, in order.
    pub fn rows(&self) -> Vec<FileRow> {
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

    /// Whether the folder has nothing in it to show. The page above puts an
    /// empty state in its place rather than an empty box.
    pub fn is_empty(&self) -> bool {
        self.imp()
            .roots
            .borrow()
            .as_ref()
            .map_or(true, |roots| roots.n_items() == 0)
    }
}

/// What is in a directory, folders first and then files, each by name.
///
/// Hidden entries are left out: a project folder that is a git checkout is
/// mostly `.git`, and nobody opened this to look at it.
fn entries_of(path: &Path) -> Vec<Node> {
    let Ok(entries) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    let mut folders = Vec::new();
    let mut files = Vec::new();
    for entry in entries.flatten().take(MAX_ENTRIES * 4) {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            folders.push((name, path));
        } else {
            files.push((name, path));
        }
    }
    let by_name =
        |a: &(String, PathBuf), b: &(String, PathBuf)| a.0.to_lowercase().cmp(&b.0.to_lowercase());
    folders.sort_by(by_name);
    files.sort_by(by_name);

    let mut nodes = Vec::new();
    for (name, path) in folders {
        nodes.push(Node::new(
            FileRow::Folder(path),
            &name,
            gio::ThemedIcon::new("folder-symbolic").upcast(),
        ));
    }
    for (name, path) in files {
        // The icon the file manager would show, so a spreadsheet looks like a
        // spreadsheet. Guessed from the name alone: reading the first bytes of
        // every file in a folder to draw a list is not a trade worth making.
        let icon =
            gio::content_type_get_symbolic_icon(&gio::content_type_guess(Some(&name), None).0);
        nodes.push(Node::new(FileRow::File(path), &name, icon));
    }
    nodes.truncate(MAX_ENTRIES);
    nodes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_tells_a_folder_from_a_file_of_the_same_name() {
        let path = PathBuf::from("/home/someone/notes");
        assert_ne!(
            FileRow::Folder(path.clone()).key(),
            FileRow::File(path).key()
        );
    }

    #[test]
    fn a_folder_offers_a_way_to_make_another_and_a_file_does_not() {
        let has = |menu: &gio::Menu, action: &str| {
            use gtk::prelude::*;
            let model: &gio::MenuModel = menu.upcast_ref();
            (0..model.n_items()).any(|index| {
                model
                    .item_attribute_value(index, gio::MENU_ATTRIBUTE_ACTION, None)
                    .and_then(|value| value.get::<String>())
                    .is_some_and(|named| named == action)
            })
        };
        let folder = FileRow::Folder(PathBuf::from("/tmp/x")).menu();
        let file = FileRow::File(PathBuf::from("/tmp/x.md")).menu();
        assert!(has(&folder, "file.new-folder"));
        assert!(!has(&file, "file.new-folder"));
        assert!(has(&file, "file.open"));
    }
}
