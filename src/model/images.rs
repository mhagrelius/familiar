//! Images you attach to a question.
//!
//! An image is content-addressed: the file is named by the SHA-256 of its
//! bytes, so pasting the same screenshot into three threads stores it once and
//! a thread file refers to it by name. That also makes the reference stable —
//! nothing renames, so nothing dangles.
//!
//! They are **untrusted data**, and unlike text there is nothing to scrub: you
//! cannot strip an instruction out of a picture of one. The mitigation is
//! structural and lives elsewhere — an image can only ever reach the model as
//! part of a question you asked, and the tools that act on the world are gated.
//!
//! Storage is beside the thread that refers to it, under the context, so
//! deleting a context takes its images with it.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// What the model is sent, and what the composer shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// `<sha256>.<extension>`, which is both the file name and the identity.
    pub name: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
}

impl Attachment {
    /// Take some bytes and work out what they are.
    ///
    /// The format is read from the bytes rather than from a file extension: a
    /// clipboard grab has no name, and a `.png` that is really a JPEG would be
    /// described wrongly to the model.
    pub fn new(bytes: Vec<u8>, digest: impl FnOnce(&[u8]) -> String) -> Option<Self> {
        let media_type = sniff(&bytes)?;
        let extension = media_type.rsplit('/').next().unwrap_or("png");
        let name = format!("{}.{extension}", digest(&bytes));
        Some(Self {
            name,
            media_type: media_type.to_string(),
            bytes,
        })
    }

    /// The `data:` URL llama-server decodes and hands to the projector.
    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.media_type, base64(&self.bytes))
    }

    /// Roughly how much of the model's context this will cost, for the UI to
    /// warn with. A rule of thumb, not a promise: the true cost depends on the
    /// projector's patch size.
    pub fn approximate_tokens(&self) -> usize {
        // ~750 tokens for a typical screenshot at this model's resolution.
        (self.bytes.len() / 1400).clamp(200, 3000)
    }
}

/// What kind of image these bytes are, by their magic number.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    if bytes.starts_with(PNG) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() > 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// Where a context keeps the images its threads refer to.
pub fn directory(context: &Path) -> PathBuf {
    context.join("images")
}

/// Write an attachment, or notice it is already there.
///
/// Content-addressed, so writing the same image twice is a no-op — which is
/// what makes pasting the same screenshot into several threads cheap.
pub fn store(context: &Path, attachment: &Attachment) -> io::Result<PathBuf> {
    let directory = directory(context);
    fs::create_dir_all(&directory)?;
    let path = directory.join(&attachment.name);
    if path.exists() {
        return Ok(path);
    }

    let temporary = path.with_extension("part");
    let mut file = fs::File::create(&temporary)?;
    file.write_all(&attachment.bytes)?;
    file.flush()?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, &path)?;
    Ok(path)
}

/// Read one back, for replaying a thread or sending it again.
pub fn load(context: &Path, name: &str) -> io::Result<Attachment> {
    // The name is content-addressed and generated here, but it arrives from a
    // file on disk — so it is still checked before it becomes a path.
    if !is_safe_name(name) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} is not an image reference"),
        ));
    }
    let path = directory(context).join(name);
    let bytes = fs::read(&path)?;
    let media_type = sniff(&bytes).unwrap_or("image/png").to_string();
    Ok(Attachment {
        name: name.to_string(),
        media_type,
        bytes,
    })
}

/// A name that can only ever be a file in the images directory.
fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 96
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        && !name.starts_with('.')
        && !name.contains("..")
}

/// Base64, written out because pulling in a crate for forty lines of table
/// lookup is not a dependency worth having.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let block = match chunk.len() {
            3 => u32::from(chunk[0]) << 16 | u32::from(chunk[1]) << 8 | u32::from(chunk[2]),
            2 => u32::from(chunk[0]) << 16 | u32::from(chunk[1]) << 8,
            _ => u32::from(chunk[0]) << 16,
        };
        for shift in 0..4 {
            if shift <= chunk.len() {
                let index = (block >> (18 - shift * 6)) & 0x3f;
                out.push(ALPHABET[index as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-pixel PNG.
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    fn digest(bytes: &[u8]) -> String {
        // A stand-in for GLib's SHA-256 in tests: the property under test is
        // that the name comes from the bytes, not which hash it is.
        format!(
            "{:016x}",
            bytes
                .iter()
                .fold(0u64, |a, b| a.wrapping_mul(31).wrapping_add(u64::from(*b)))
        )
    }

    #[test]
    fn base64_matches_the_definition() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn the_format_is_read_from_the_bytes_not_the_name() {
        assert_eq!(sniff(PNG), Some("image/png"));
        assert_eq!(sniff(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
        assert_eq!(sniff(b"GIF89a..."), Some("image/gif"));
        assert_eq!(sniff(b"not an image"), None);
    }

    #[test]
    fn something_that_is_not_an_image_is_not_attached() {
        assert!(Attachment::new(b"a text file".to_vec(), digest).is_none());
    }

    #[test]
    fn the_same_image_gets_the_same_name_and_is_stored_once() {
        let directory = tempfile::tempdir().expect("temp dir");
        let first = Attachment::new(PNG.to_vec(), digest).expect("an image");
        let second = Attachment::new(PNG.to_vec(), digest).expect("an image");
        assert_eq!(first.name, second.name);
        assert!(first.name.ends_with(".png"), "{}", first.name);

        let path = store(directory.path(), &first).expect("store");
        let again = store(directory.path(), &second).expect("store");
        assert_eq!(path, again);
        assert_eq!(
            fs::read_dir(super::directory(directory.path()))
                .expect("dir")
                .count(),
            1
        );
    }

    #[test]
    fn an_image_round_trips_through_the_disk() {
        let directory = tempfile::tempdir().expect("temp dir");
        let attachment = Attachment::new(PNG.to_vec(), digest).expect("an image");
        store(directory.path(), &attachment).expect("store");

        let read = load(directory.path(), &attachment.name).expect("load");
        assert_eq!(read, attachment);
    }

    #[test]
    fn a_reference_cannot_climb_out_of_the_images_directory() {
        let directory = tempfile::tempdir().expect("temp dir");
        for name in ["../../../etc/passwd", ".hidden", "a/b.png", ""] {
            assert!(load(directory.path(), name).is_err(), "{name}");
        }
    }

    #[test]
    fn the_data_url_says_what_the_bytes_are() {
        let attachment = Attachment::new(PNG.to_vec(), digest).expect("an image");
        let url = attachment.data_url();
        assert!(url.starts_with("data:image/png;base64,"), "{url}");
        assert!(url.len() > 40, "{url}");
    }

    #[test]
    fn storing_leaves_no_partial_file_behind() {
        let directory = tempfile::tempdir().expect("temp dir");
        let attachment = Attachment::new(PNG.to_vec(), digest).expect("an image");
        store(directory.path(), &attachment).expect("store");
        let leftovers = fs::read_dir(super::directory(directory.path()))
            .expect("dir")
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|e| e == "part"))
            .count();
        assert_eq!(leftovers, 0);
    }
}
