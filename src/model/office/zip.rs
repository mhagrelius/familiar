//! Just enough ZIP to make an Office file.
//!
//! `.docx`, `.xlsx` and `.pptx` are ZIP archives of XML parts, and the only
//! thing standing between "we can write XML" and "we can write a Word file" is
//! an archive writer. This is that writer and nothing more.
//!
//! **Entries are stored, not deflated.** The format allows it, and every reader
//! that matters — Word, Excel, PowerPoint, LibreOffice, Google Docs, Pages —
//! accepts it, because method 0 has been in the specification since 1989. The
//! alternative was a DEFLATE compressor, which is either a new dependency or
//! several hundred lines of Huffman coding to save some kilobytes on documents
//! that are kilobytes. A ten-slide deck is ~40 KB stored; nobody notices.
//!
//! There is no reader here on purpose. Reading a `.docx` someone else wrote
//! means inflating it, which is the compressor problem again, and Familiar has
//! no tool that needs to.

/// One file inside the archive.
struct Entry {
    name: String,
    crc: u32,
    size: u32,
    /// Where this entry's local header starts, which is what the central
    /// directory has to point at.
    offset: u32,
}

/// Builds a ZIP in memory, one part at a time.
#[derive(Default)]
pub struct Archive {
    bytes: Vec<u8>,
    entries: Vec<Entry>,
}

impl Archive {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a part. Names are archive paths with forward slashes, always —
    /// a backslash makes a file Word will open and Google Docs will not.
    pub fn add(&mut self, name: &str, contents: &[u8]) {
        let name = name.replace('\\', "/");
        let crc = crc32(contents);
        let offset = self.bytes.len() as u32;

        self.bytes.extend_from_slice(&0x0403_4b50u32.to_le_bytes()); // local header
        self.bytes.extend_from_slice(&20u16.to_le_bytes()); // version needed
        self.bytes.extend_from_slice(&0u16.to_le_bytes()); // flags
        self.bytes.extend_from_slice(&0u16.to_le_bytes()); // method: stored
        self.bytes.extend_from_slice(&DOS_TIME.to_le_bytes());
        self.bytes.extend_from_slice(&DOS_DATE.to_le_bytes());
        self.bytes.extend_from_slice(&crc.to_le_bytes());
        self.bytes
            .extend_from_slice(&(contents.len() as u32).to_le_bytes());
        self.bytes
            .extend_from_slice(&(contents.len() as u32).to_le_bytes());
        self.bytes
            .extend_from_slice(&(name.len() as u16).to_le_bytes());
        self.bytes.extend_from_slice(&0u16.to_le_bytes()); // extra field length
        self.bytes.extend_from_slice(name.as_bytes());
        self.bytes.extend_from_slice(contents);

        self.entries.push(Entry {
            name,
            crc,
            size: contents.len() as u32,
            offset,
        });
    }

    /// Convenience for the common case: a part that is text.
    pub fn add_text(&mut self, name: &str, contents: &str) {
        self.add(name, contents.as_bytes());
    }

    /// Close the archive: write the central directory and hand back the bytes.
    pub fn finish(mut self) -> Vec<u8> {
        let directory_start = self.bytes.len() as u32;

        for entry in &self.entries {
            self.bytes.extend_from_slice(&0x0201_4b50u32.to_le_bytes()); // central header
            self.bytes.extend_from_slice(&20u16.to_le_bytes()); // version made by
            self.bytes.extend_from_slice(&20u16.to_le_bytes()); // version needed
            self.bytes.extend_from_slice(&0u16.to_le_bytes()); // flags
            self.bytes.extend_from_slice(&0u16.to_le_bytes()); // method: stored
            self.bytes.extend_from_slice(&DOS_TIME.to_le_bytes());
            self.bytes.extend_from_slice(&DOS_DATE.to_le_bytes());
            self.bytes.extend_from_slice(&entry.crc.to_le_bytes());
            self.bytes.extend_from_slice(&entry.size.to_le_bytes());
            self.bytes.extend_from_slice(&entry.size.to_le_bytes());
            self.bytes
                .extend_from_slice(&(entry.name.len() as u16).to_le_bytes());
            self.bytes.extend_from_slice(&0u16.to_le_bytes()); // extra
            self.bytes.extend_from_slice(&0u16.to_le_bytes()); // comment
            self.bytes.extend_from_slice(&0u16.to_le_bytes()); // disk number
            self.bytes.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
            self.bytes.extend_from_slice(&0u32.to_le_bytes()); // external attrs
            self.bytes.extend_from_slice(&entry.offset.to_le_bytes());
            self.bytes.extend_from_slice(entry.name.as_bytes());
        }

        let directory_size = self.bytes.len() as u32 - directory_start;
        let count = self.entries.len() as u16;

        self.bytes.extend_from_slice(&0x0605_4b50u32.to_le_bytes()); // end of directory
        self.bytes.extend_from_slice(&0u16.to_le_bytes()); // this disk
        self.bytes.extend_from_slice(&0u16.to_le_bytes()); // disk with directory
        self.bytes.extend_from_slice(&count.to_le_bytes());
        self.bytes.extend_from_slice(&count.to_le_bytes());
        self.bytes.extend_from_slice(&directory_size.to_le_bytes());
        self.bytes.extend_from_slice(&directory_start.to_le_bytes());
        self.bytes.extend_from_slice(&0u16.to_le_bytes()); // comment length

        self.bytes
    }
}

/// A fixed timestamp — 1980-01-01 00:00:00, the earliest a DOS date can hold.
///
/// The same document written twice is then byte-identical, which is what makes
/// the tests here assertions about content rather than about the clock.
const DOS_TIME: u16 = 0;
const DOS_DATE: u16 = 0x0021;

/// CRC-32, the ZIP polynomial, computed a nibble at a time.
///
/// The table is 16 entries built on the fly rather than the usual 256 baked in:
/// a document is a handful of small parts, and this is not where the time goes.
fn crc32(bytes: &[u8]) -> u32 {
    const POLYNOMIAL: u32 = 0xEDB8_8320;
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ POLYNOMIAL
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The published check value for CRC-32: "123456789" is 0xCBF43926.
    #[test]
    fn the_checksum_matches_the_published_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn an_archive_starts_with_a_local_header_and_ends_with_the_directory() {
        let mut archive = Archive::new();
        archive.add_text("[Content_Types].xml", "<Types/>");
        let bytes = archive.finish();

        assert_eq!(&bytes[..4], b"PK\x03\x04");
        assert_eq!(&bytes[bytes.len() - 22..bytes.len() - 18], b"PK\x05\x06");
    }

    #[test]
    fn the_end_record_points_at_a_directory_holding_every_entry() {
        let mut archive = Archive::new();
        archive.add_text("one.xml", "<a/>");
        archive.add_text("two/three.xml", "<b/>");
        let bytes = archive.finish();

        let end = bytes.len() - 22;
        let count = u16::from_le_bytes([bytes[end + 10], bytes[end + 11]]);
        let size = u32::from_le_bytes(bytes[end + 12..end + 16].try_into().expect("4 bytes"));
        let start = u32::from_le_bytes(bytes[end + 16..end + 20].try_into().expect("4 bytes"));

        assert_eq!(count, 2);
        assert_eq!(start as usize + size as usize, end);
        assert_eq!(&bytes[start as usize..start as usize + 4], b"PK\x01\x02");
    }

    #[test]
    fn every_entry_is_findable_at_the_offset_the_directory_gives() {
        // The one thing a reader cannot recover from: an offset that does not
        // land on a local header means the part is unreachable.
        let mut archive = Archive::new();
        archive.add_text("word/document.xml", "<w:document/>");
        archive.add_text("_rels/.rels", "<Relationships/>");
        let bytes = archive.finish();

        let end = bytes.len() - 22;
        let mut cursor =
            u32::from_le_bytes(bytes[end + 16..end + 20].try_into().expect("4 bytes")) as usize;
        for _ in 0..2 {
            assert_eq!(&bytes[cursor..cursor + 4], b"PK\x01\x02");
            let name_length = u16::from_le_bytes([bytes[cursor + 28], bytes[cursor + 29]]) as usize;
            let offset =
                u32::from_le_bytes(bytes[cursor + 42..cursor + 46].try_into().expect("4 bytes"))
                    as usize;
            assert_eq!(&bytes[offset..offset + 4], b"PK\x03\x04");
            cursor += 46 + name_length;
        }
    }

    #[test]
    fn a_stored_entry_holds_its_contents_verbatim() {
        let mut archive = Archive::new();
        archive.add_text("word/document.xml", "<w:t>hello</w:t>");
        let bytes = archive.finish();
        let found = bytes
            .windows(16)
            .any(|window| window == b"<w:t>hello</w:t>");
        assert!(found, "the part is not in the archive as written");
    }

    #[test]
    fn the_same_document_written_twice_is_byte_identical() {
        // No clock in the headers, so a test can assert on content.
        let build = || {
            let mut archive = Archive::new();
            archive.add_text("a.xml", "<a/>");
            archive.finish()
        };
        assert_eq!(build(), build());
    }

    #[test]
    fn part_names_always_use_forward_slashes() {
        let mut archive = Archive::new();
        archive.add_text("word\\document.xml", "<a/>");
        let bytes = archive.finish();
        assert!(bytes
            .windows(17)
            .any(|window| window == b"word/document.xml"));
    }
}
