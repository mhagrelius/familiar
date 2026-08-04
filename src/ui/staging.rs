//! Images waiting to be sent with the next question.
//!
//! Paste one and it appears as a thumbnail under the entry until you send or
//! remove it. That waiting room is the whole point: an image you attached and
//! cannot see is one you cannot take back, and pasting into a chat box is easy
//! to do by accident.

use std::cell::RefCell;
use std::sync::OnceLock;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib::subclass::Signal;
use gtk::glib::{self, clone};

use crate::model::images::Attachment;

/// The largest thing worth sending. A phone photograph is tens of megabytes of
/// detail the projector immediately throws away, and the base64 of it would
/// cost more to transfer than the answer is worth.
pub const MAX_BYTES: usize = 12 * 1024 * 1024;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Staging {
        pub row: RefCell<Option<adw::WrapBox>>,
        pub attachments: RefCell<Vec<Attachment>>,
        /// Documents whose text was extracted rather than rendered: name and
        /// contents, already framed.
        pub documents: RefCell<Vec<(String, String)>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Staging {
        const NAME: &'static str = "FamiliarStaging";
        type Type = super::Staging;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_layout_manager_type::<gtk::BinLayout>();
        }
    }

    impl ObjectImpl for Staging {
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
            SIGNALS.get_or_init(|| vec![Signal::builder("changed").build()])
        }
    }

    impl WidgetImpl for Staging {}
}

glib::wrapper! {
    pub struct Staging(ObjectSubclass<imp::Staging>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for Staging {
    fn default() -> Self {
        Self::new()
    }
}

impl Staging {
    pub fn new() -> Self {
        glib::Object::new()
    }

    fn build(&self) {
        let row = adw::WrapBox::new();
        row.set_child_spacing(6);
        row.set_line_spacing(6);
        row.set_margin_bottom(6);
        row.set_parent(self);
        self.set_visible(false);
        self.imp().row.replace(Some(row));
    }

    /// Add an image. Refuses anything that is not one, and anything enormous.
    ///
    /// Returns what to tell the user, or `None` when it was added without
    /// comment — the thumbnail is the confirmation.
    pub fn add(&self, bytes: Vec<u8>) -> Option<String> {
        if bytes.len() > MAX_BYTES {
            return Some(format!(
                "That image is {} MB. Images are limited to {} MB.",
                bytes.len() / 1_048_576,
                MAX_BYTES / 1_048_576
            ));
        }
        let Some(attachment) = Attachment::new(bytes, digest) else {
            return Some("That is not an image.".into());
        };

        // Content-addressed, so the same picture twice is the same picture.
        if self
            .imp()
            .attachments
            .borrow()
            .iter()
            .any(|held| held.name == attachment.name)
        {
            return Some("That image is already attached.".into());
        }

        self.imp().attachments.borrow_mut().push(attachment);
        self.redraw();
        None
    }

    /// Attach a document by its text rather than its pixels.
    ///
    /// What comes back from `pdftotext` when the PDF has a text layer: exact,
    /// and a fraction of the tokens the same pages would cost as images.
    pub fn add_document(&self, name: &str, framed: String) {
        self.imp()
            .documents
            .borrow_mut()
            .push((name.to_string(), framed));
        self.redraw();
    }

    /// Take the images, leaving the staging area empty. Called when the
    /// question is sent.
    pub fn take(&self) -> Vec<Attachment> {
        let taken = std::mem::take(&mut *self.imp().attachments.borrow_mut());
        self.redraw();
        taken
    }

    /// Take the documents' text, to go into the question.
    pub fn take_documents(&self) -> Vec<String> {
        let taken = std::mem::take(&mut *self.imp().documents.borrow_mut());
        self.redraw();
        taken.into_iter().map(|(_, framed)| framed).collect()
    }

    pub fn clear(&self) {
        self.imp().attachments.borrow_mut().clear();
        self.redraw();
    }

    pub fn is_empty(&self) -> bool {
        self.imp().attachments.borrow().is_empty() && self.imp().documents.borrow().is_empty()
    }

    pub fn len(&self) -> usize {
        self.imp().attachments.borrow().len()
    }

    fn remove(&self, name: &str) {
        self.imp()
            .attachments
            .borrow_mut()
            .retain(|held| held.name != name);
        self.redraw();
    }

    fn redraw(&self) {
        let imp = self.imp();
        let Some(row) = imp.row.borrow().clone() else {
            return;
        };
        while let Some(child) = row.first_child() {
            row.remove(&child);
        }

        let attachments = imp.attachments.borrow().clone();
        for attachment in &attachments {
            row.append(&self.thumbnail(attachment));
        }
        let documents = imp.documents.borrow().clone();
        for (name, framed) in &documents {
            row.append(&self.document_chip(name, framed.len()));
        }
        self.set_visible(!attachments.is_empty() || !documents.is_empty());
        self.emit_by_name::<()>("changed", &[]);
    }

    /// One thumbnail with a remove button on it.
    fn thumbnail(&self, attachment: &Attachment) -> gtk::Widget {
        let picture = gtk::Picture::new();
        picture.set_content_fit(gtk::ContentFit::Cover);
        picture.set_size_request(64, 64);
        if let Ok(texture) = gtk::gdk::Texture::from_bytes(&glib::Bytes::from(&attachment.bytes)) {
            picture.set_paintable(Some(&texture));
        }

        let frame = gtk::Frame::new(None);
        frame.add_css_class("staged-image");
        frame.set_child(Some(&picture));

        let remove = gtk::Button::from_icon_name("window-close-symbolic");
        remove.add_css_class("osd");
        remove.add_css_class("circular");
        remove.set_halign(gtk::Align::End);
        remove.set_valign(gtk::Align::Start);
        remove.set_tooltip_text(Some("Remove"));
        remove.connect_clicked(clone!(
            #[weak(rename_to = staging)]
            self,
            #[strong(rename_to = name)]
            attachment.name,
            move |_| staging.remove(&name)
        ));

        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&frame));
        overlay.add_overlay(&remove);
        overlay.set_tooltip_text(Some(&format!(
            "{} · about {} tokens",
            attachment.media_type,
            attachment.approximate_tokens()
        )));
        overlay.upcast()
    }
}

impl Staging {
    /// A document reads as a chip rather than a thumbnail: there is no picture
    /// of it, and its size is the thing worth knowing.
    fn document_chip(&self, name: &str, characters: usize) -> gtk::Widget {
        let label = gtk::Label::new(Some(&format!("{name} · {} chars", characters)));
        label.add_css_class("caption");
        label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        label.set_max_width_chars(32);

        let image = gtk::Image::from_icon_name("x-office-document-symbolic");
        image.set_pixel_size(12);

        let chip = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        chip.add_css_class("tool-chip");
        chip.append(&image);
        chip.append(&label);

        let remove = gtk::Button::from_icon_name("window-close-symbolic");
        remove.add_css_class("flat");
        remove.add_css_class("circular");
        remove.set_tooltip_text(Some("Remove"));
        remove.connect_clicked(clone!(
            #[weak(rename_to = staging)]
            self,
            #[strong(rename_to = name)]
            name.to_string(),
            move |_| {
                staging
                    .imp()
                    .documents
                    .borrow_mut()
                    .retain(|(held, _)| *held != name);
                staging.redraw();
            }
        ));
        chip.append(&remove);
        chip.upcast()
    }
}

/// SHA-256, from GLib rather than a crate: it is already linked and this is the
/// one place a hash is needed.
pub fn digest(bytes: &[u8]) -> String {
    glib::compute_checksum_for_bytes(glib::ChecksumType::Sha256, &glib::Bytes::from(bytes))
        .map(|sum| sum.to_string())
        .unwrap_or_default()
}
