//! The entry, and the one button that sends or stops.
//!
//! Enter sends and Ctrl+Enter or Shift+Enter starts a line, which is the way
//! round every chat client has taught people to expect. The button is the same
//! button throughout a turn: it sends, then it stops, so the thing that started
//! the generation is also the thing that ends it and there is no second control
//! to find.

use std::sync::OnceLock;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib::subclass::Signal;
use gtk::glib::{self, clone};

use crate::model::documents;
use crate::ui::Staging;

/// Past this the entry scrolls instead of growing; a long paste should not eat
/// the conversation.
const MAX_HEIGHT: i32 = 160;

/// Run a command and hand back its standard output, or `None` if it could not
/// be started. On the main loop, so nothing blocks while poppler works.
fn run<F>(command: Vec<String>, done: F)
where
    F: FnOnce(Option<String>) + 'static,
{
    let arguments: Vec<&std::ffi::OsStr> = command.iter().map(std::ffi::OsStr::new).collect();
    let launcher = gtk::gio::SubprocessLauncher::new(
        gtk::gio::SubprocessFlags::STDOUT_PIPE | gtk::gio::SubprocessFlags::STDERR_SILENCE,
    );
    let Ok(process) = launcher.spawn(&arguments) else {
        done(None);
        return;
    };
    process.communicate_utf8_async(None, gtk::gio::Cancellable::NONE, move |result| {
        done(
            result
                .ok()
                .and_then(|(out, _)| out)
                .map(|out| out.to_string()),
        );
    });
}

mod imp {
    use super::*;

    pub struct Composer {
        pub staging: Staging,
        pub view: gtk::TextView,
        pub placeholder: gtk::Label,
        pub attach: gtk::Button,
        pub button: gtk::Button,
        pub busy: std::cell::Cell<bool>,
    }

    impl Default for Composer {
        fn default() -> Self {
            Self {
                staging: Staging::new(),
                view: gtk::TextView::new(),
                placeholder: gtk::Label::new(Some("Ask something…")),
                attach: gtk::Button::new(),
                button: gtk::Button::new(),
                busy: std::cell::Cell::new(false),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Composer {
        const NAME: &'static str = "FamiliarComposer";
        type Type = super::Composer;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_layout_manager_type::<gtk::BinLayout>();
        }
    }

    impl ObjectImpl for Composer {
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
                    Signal::builder("submit")
                        .param_types([String::static_type()])
                        .build(),
                    Signal::builder("stop").build(),
                    // Something was pasted or dropped that is not an image, or
                    // is too big — the window says so.
                    Signal::builder("complain")
                        .param_types([String::static_type()])
                        .build(),
                ]
            })
        }
    }

    impl WidgetImpl for Composer {}
}

glib::wrapper! {
    pub struct Composer(ObjectSubclass<imp::Composer>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for Composer {
    fn default() -> Self {
        Self::new()
    }
}

impl Composer {
    pub fn new() -> Self {
        glib::Object::new()
    }

    fn build(&self) {
        let imp = self.imp();

        imp.view.set_wrap_mode(gtk::WrapMode::WordChar);
        imp.view.set_top_margin(10);
        imp.view.set_bottom_margin(10);
        imp.view.set_left_margin(10);
        imp.view.set_right_margin(10);
        imp.view.set_accepts_tab(false);
        imp.view.add_css_class("composer-entry");

        // A TextView has no placeholder, so one is drawn over it and hidden as
        // soon as there is anything to read.
        imp.placeholder.set_halign(gtk::Align::Start);
        imp.placeholder.set_valign(gtk::Align::Start);
        imp.placeholder.set_xalign(0.0);
        imp.placeholder.set_margin_top(10);
        imp.placeholder.set_margin_start(12);
        imp.placeholder.add_css_class("dimmed");
        imp.placeholder.set_can_target(false);

        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&imp.view));
        overlay.add_overlay(&imp.placeholder);

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(MAX_HEIGHT)
            .hexpand(true)
            .child(&overlay)
            .build();

        // Pasting and dropping both attach a file, but neither of them is
        // visible: without a control, nothing on screen says a question can
        // carry a picture or a PDF at all.
        imp.attach.set_icon_name("list-add-symbolic");
        imp.attach.set_tooltip_text(Some("Attach a File"));
        imp.attach.set_valign(gtk::Align::End);
        imp.attach.add_css_class("flat");
        imp.attach.add_css_class("circular");

        imp.button.set_icon_name("document-send-symbolic");
        imp.button.set_tooltip_text(Some("Send"));
        imp.button.set_valign(gtk::Align::End);
        imp.button.add_css_class("suggested-action");
        imp.button.add_css_class("circular");
        imp.button.set_sensitive(false);

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.add_css_class("card");
        row.add_css_class("composer");
        row.append(&imp.attach);
        row.append(&scroller);
        row.append(&imp.button);

        let holder = gtk::Box::new(gtk::Orientation::Vertical, 0);
        holder.set_margin_top(6);
        holder.set_margin_bottom(12);
        holder.set_margin_start(12);
        holder.set_margin_end(12);
        holder.append(&imp.staging);
        holder.append(&row);

        let clamp = adw::Clamp::builder()
            .maximum_size(760)
            .tightening_threshold(600)
            .child(&holder)
            .build();
        clamp.set_parent(self);

        // An image arriving or leaving changes whether there is anything to
        // send, the same as typing does.
        imp.staging.connect_closure(
            "changed",
            false,
            glib::closure_local!(
                #[watch(rename_to = composer)]
                self,
                move |_: Staging| {
                    let imp = composer.imp();
                    let empty = imp.view.buffer().char_count() == 0;
                    imp.button
                        .set_sensitive(imp.busy.get() || !empty || !imp.staging.is_empty());
                }
            ),
        );

        imp.view.buffer().connect_changed(clone!(
            #[weak(rename_to = composer)]
            self,
            move |buffer| {
                let empty = buffer.char_count() == 0;
                let imp = composer.imp();
                imp.placeholder.set_visible(empty);
                // Sending nothing is not a thing you can do — but an image with
                // no words is not nothing. Stopping is always allowed.
                imp.button
                    .set_sensitive(imp.busy.get() || !empty || !imp.staging.is_empty());
            }
        ));

        // Ctrl+V with an image on the clipboard. The TextView's own paste
        // handles text; this runs first and only claims the event when what is
        // on the clipboard is a picture.
        let paste = gtk::EventControllerKey::new();
        paste.set_propagation_phase(gtk::PropagationPhase::Capture);
        paste.connect_key_pressed(clone!(
            #[weak(rename_to = composer)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, key, _, modifiers| {
                let pasting = modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK)
                    && matches!(key, gtk::gdk::Key::v | gtk::gdk::Key::V);
                if pasting && composer.paste_image() {
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            }
        ));
        imp.view.add_controller(paste);

        // Dropping a file works the same way, and is how most people attach a
        // screenshot they have already saved.
        let drop = gtk::DropTarget::new(gtk::gio::File::static_type(), gtk::gdk::DragAction::COPY);
        drop.connect_drop(clone!(
            #[weak(rename_to = composer)]
            self,
            #[upgrade_or]
            false,
            move |_, value, _, _| {
                let Ok(file) = value.get::<gtk::gio::File>() else {
                    return false;
                };
                composer.attach_file(&file);
                true
            }
        ));
        row.add_controller(drop);

        imp.attach.connect_clicked(clone!(
            #[weak(rename_to = composer)]
            self,
            move |_| composer.choose_file()
        ));

        imp.button.connect_clicked(clone!(
            #[weak(rename_to = composer)]
            self,
            move |_| composer.fire()
        ));

        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(clone!(
            #[weak(rename_to = composer)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, key, _, modifiers| {
                let enter = matches!(key, gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter);
                let plain = !modifiers.intersects(
                    gtk::gdk::ModifierType::CONTROL_MASK | gtk::gdk::ModifierType::SHIFT_MASK,
                );
                if enter && plain {
                    // Not `fire`: while a turn streams the *button* means stop,
                    // and pressing Return to send the next question must never
                    // silently cancel the answer being written.
                    composer.send();
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            }
        ));
        imp.view.add_controller(keys);
    }

    /// Send, or stop — whichever the button currently means.
    fn fire(&self) {
        if self.imp().busy.get() {
            self.emit_by_name::<()>("stop", &[]);
            return;
        }
        self.send();
    }

    /// Send what is typed, if anything, and if there is nothing already
    /// running. This is what Return does.
    pub fn send(&self) {
        if self.imp().busy.get() {
            return;
        }
        let text = self.text();
        // An image with no words is a question — "what is this?" is implied.
        if text.trim().is_empty() && self.imp().staging.is_empty() {
            return;
        }
        self.clear();
        self.emit_by_name::<()>("submit", &[&text]);
    }

    /// The images waiting to go with the next question.
    pub fn staging(&self) -> Staging {
        self.imp().staging.clone()
    }

    /// Read an image off the clipboard, if there is one. Reports whether it
    /// claimed the paste.
    fn paste_image(&self) -> bool {
        let Some(display) = gtk::gdk::Display::default() else {
            return false;
        };
        let clipboard = display.clipboard();
        let formats = clipboard.formats();
        if !formats.contains_type(gtk::gdk::Texture::static_type())
            && !formats.contain_mime_type("image/png")
        {
            return false;
        }

        clipboard.read_texture_async(
            gtk::gio::Cancellable::NONE,
            clone!(
                #[weak(rename_to = composer)]
                self,
                move |result| {
                    let Ok(Some(texture)) = result else {
                        composer.complain("There is no image on the clipboard.");
                        return;
                    };
                    // PNG rather than the raw texture: it is what the model is
                    // sent, and encoding once here beats encoding on every turn.
                    composer.attach(texture.save_to_png_bytes().to_vec());
                }
            ),
        );
        true
    }

    /// Pick a file to attach. The same road a drop takes, so a PDF chosen here
    /// is ingested exactly as one dragged in.
    fn choose_file(&self) {
        let pictures = gtk::FileFilter::new();
        pictures.set_name(Some("Images and PDFs"));
        pictures.add_mime_type("image/*");
        pictures.add_mime_type("application/pdf");
        let everything = gtk::FileFilter::new();
        everything.set_name(Some("All Files"));
        everything.add_pattern("*");

        let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&pictures);
        filters.append(&everything);

        let dialog = gtk::FileDialog::builder()
            .title("Attach a File")
            .filters(&filters)
            .default_filter(&pictures)
            .modal(true)
            .build();

        let parent = self.root().and_downcast::<gtk::Window>();
        dialog.open(
            parent.as_ref(),
            gtk::gio::Cancellable::NONE,
            clone!(
                #[weak(rename_to = composer)]
                self,
                move |result| {
                    // Cancelling is not a failure and has nothing to say.
                    if let Ok(file) = result {
                        composer.attach_file(&file);
                    }
                }
            ),
        );
    }

    fn attach_file(&self, file: &gtk::gio::File) {
        let bytes = match file.load_contents(gtk::gio::Cancellable::NONE) {
            Ok((bytes, _)) => bytes.to_vec(),
            Err(error) => {
                self.complain(&format!("That file could not be read: {error}"));
                return;
            }
        };

        // llama-server will not take a PDF — a data:application/pdf URL comes
        // back as "Invalid uri format" — so one has to become either text or
        // pictures. Text where there is text: it is exact, instant, and costs a
        // fraction of what the same pages cost as images.
        if documents::is_pdf(&bytes) {
            self.ingest_document(file);
            return;
        }
        self.attach(bytes);
    }

    /// Ingest a PDF: ask what it is, extract what has words, and look at the
    /// rest.
    ///
    /// Three subprocesses at most, all through gio so the main loop keeps
    /// running — a fifty-page document would otherwise freeze the window.
    fn ingest_document(&self, file: &gtk::gio::File) {
        let Some(path) = file.path() else {
            self.complain("That document is not a local file.");
            return;
        };
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "document.pdf".into());

        // What is it? The page count is what tells us a page went missing
        // during extraction, which is exactly a page that needs looking at.
        run(
            documents::info_command(&path),
            clone!(
                #[weak(rename_to = composer)]
                self,
                #[strong]
                path,
                #[strong]
                name,
                move |counted: Option<String>| {
                    let Some(counted) = counted else {
                        composer.complain(
                            "Reading PDFs needs poppler-utils (pdfinfo, pdftotext, pdftoppm).",
                        );
                        return;
                    };
                    let info = documents::parse_info(&counted);

                    run(
                        documents::extract_command(&path),
                        clone!(
                            #[weak]
                            composer,
                            #[strong]
                            path,
                            #[strong]
                            name,
                            move |extracted: Option<String>| {
                                let pages = documents::split_pages(&extracted.unwrap_or_default());
                                let plan = documents::plan(info.pages, &pages);

                                // The text, page by page, with the gaps named.
                                composer
                                    .imp()
                                    .staging
                                    .add_document(&name, documents::frame(&name, &info, &plan));

                                let scanned = plan.to_rasterise.len();
                                if scanned == 0 {
                                    composer.complain(&format!(
                                        "Read {name}: {} page(s) of text.",
                                        plan.pages.len()
                                    ));
                                    return;
                                }
                                composer.complain(&format!(
                            "Read {name}: {} page(s) of text, rendering {scanned} scanned page(s)…",
                            plan.pages.len() - scanned
                        ));
                                composer.rasterise(&path, plan.to_rasterise);
                            }
                        ),
                    );
                }
            ),
        );
    }

    /// Render the pages that had no text, one at a time, and stage each.
    fn rasterise(&self, path: &std::path::Path, pages: Vec<usize>) {
        let Some(page) = pages.first().copied() else {
            return;
        };
        let rest: Vec<usize> = pages.into_iter().skip(1).collect();

        let Ok(temporary) = tempfile::tempdir() else {
            self.complain("Nowhere to render that document.");
            return;
        };
        let prefix = temporary.path().join("page");
        let command = documents::rasterise_page_command(path, &prefix, page);
        let arguments: Vec<&std::ffi::OsStr> = command.iter().map(std::ffi::OsStr::new).collect();

        let launcher = gtk::gio::SubprocessLauncher::new(gtk::gio::SubprocessFlags::STDERR_SILENCE);
        let Ok(process) = launcher.spawn(&arguments) else {
            self.complain("Rendering scanned pages needs pdftoppm, from poppler-utils.");
            return;
        };

        process.wait_async(
            gtk::gio::Cancellable::NONE,
            clone!(
                #[weak(rename_to = composer)]
                self,
                #[strong(rename_to = path)]
                path.to_path_buf(),
                move |result| {
                    // The directory has to outlive the subprocess, which is why
                    // it is moved in here rather than dropped above.
                    let directory = temporary;
                    if result.is_ok() {
                        if let Some(bytes) = documents::collect_page(directory.path(), "page") {
                            composer.attach(bytes);
                        }
                    }
                    // The next page, whatever happened to this one: one bad
                    // page should not stop the rest of the document.
                    composer.rasterise(&path, rest);
                }
            ),
        );
    }

    fn attach(&self, bytes: Vec<u8>) {
        if let Some(trouble) = self.imp().staging.add(bytes) {
            self.complain(&trouble);
        }
    }

    fn complain(&self, text: &str) {
        self.emit_by_name::<()>("complain", &[&text]);
    }

    pub fn text(&self) -> String {
        let buffer = self.imp().view.buffer();
        buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .to_string()
    }

    pub fn clear(&self) {
        self.imp().view.buffer().set_text("");
    }

    pub fn focus_entry(&self) {
        self.imp().view.grab_focus();
    }

    /// A turn is in flight: the button stops it, and typing the next question
    /// while the answer streams is deliberately still allowed.
    pub fn set_busy(&self, busy: bool) {
        let imp = self.imp();
        imp.busy.set(busy);
        if busy {
            imp.button.set_icon_name("media-playback-stop-symbolic");
            imp.button.set_tooltip_text(Some("Stop"));
            imp.button.remove_css_class("suggested-action");
            imp.button.add_css_class("destructive-action");
            imp.button.set_sensitive(true);
        } else {
            imp.button.set_icon_name("document-send-symbolic");
            imp.button.set_tooltip_text(Some("Send"));
            imp.button.remove_css_class("destructive-action");
            imp.button.add_css_class("suggested-action");
            imp.button.set_sensitive(imp.view.buffer().char_count() > 0);
        }
    }

    /// The server is unreachable: there is nothing to send to. The control
    /// stays visible and goes insensitive with a reason, rather than latching
    /// while nothing happens.
    pub fn set_reachable(&self, reachable: bool, reason: Option<&str>) {
        let imp = self.imp();
        // The entry stays live. Drafting a question while the server is
        // starting up is a normal thing to be doing, and taking the keyboard
        // away mid-sentence to report someone else's problem is not.
        if reachable {
            imp.button.set_tooltip_text(Some("Send"));
            imp.button
                .set_sensitive(imp.busy.get() || imp.view.buffer().char_count() > 0);
        } else {
            imp.button.set_sensitive(false);
            imp.button.set_tooltip_text(reason);
        }
    }
}
