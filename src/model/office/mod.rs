//! Making documents: Word, Excel, PowerPoint and PDF.
//!
//! One spec, four writers. [`markup::parse`] turns the Markdown a model writes
//! into [`markup::Block`]s, and `docx`, `pptx` and `pdf` all render those same
//! blocks — which is what makes the Word file and the PDF of one document agree
//! rather than merely resemble each other. `xlsx` is the odd one out because a
//! spreadsheet is not prose; it takes rows of [`xlsx::Cell`] instead.
//!
//! The three Office formats are ZIP archives of XML, written by [`zip`] and
//! [`xml`]. The PDF is painted by Cairo and Pango, which are already linked
//! into this process. Nothing here shells out, nothing here needs a display,
//! and nothing here is a new runtime dependency — so the whole module is tested
//! the way the rest of `model/` is.
//!
//! [`skills`] is the other half of the feature: the prompt text that tells a
//! model when and how to use any of it, split into an always-loaded catalogue
//! and bodies fetched on demand.

pub mod docx;
pub mod markup;
pub mod pdf;
pub mod pptx;
pub mod skills;
pub mod xlsx;
pub mod xml;
pub mod zip;

/// The extension a path must have for the format, and what to say when it does
/// not.
///
/// A `.docx` written to `report.txt` opens in a text editor as a wall of
/// gibberish, and the user has no way to know the tool was the problem. So the
/// extension is checked rather than corrected: silently renaming what the model
/// asked for hides the mistake from both of them.
pub fn check_extension(path: &str, expected: &str) -> Result<(), String> {
    let path = path.trim();
    // Only the last segment: a directory called `q3.reports` is not an
    // extension, and `.` at the front of a hidden file is not one either.
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    if let Some((stem, found)) = name.rsplit_once('.') {
        if !stem.is_empty() && found.eq_ignore_ascii_case(expected) {
            return Ok(());
        }
    }
    // The suggestion replaces a wrong extension and appends a missing one,
    // rather than producing a bare ".docx" when there was nothing to replace.
    let suggestion = match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => {
            format!("{}{stem}.{expected}", &path[..path.len() - name.len()])
        }
        _ => format!("{path}.{expected}"),
    };
    Err(format!(
        "that path has to end in .{expected} — a {expected} file under another name will not \
         open in anything. Try {suggestion}"
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    /// Read one stored part back out of an archive.
    ///
    /// The writer stores entries uncompressed, so the bytes are in the file
    /// verbatim and a test can read exactly what Word will read — no inflater,
    /// and no second implementation of the format to disagree with the first.
    /// An absent part comes back empty, which is what the "is it there?" tests
    /// assert on.
    pub fn part(bytes: &[u8], name: &str) -> String {
        let mut cursor = 0usize;
        while cursor + 30 <= bytes.len() {
            if &bytes[cursor..cursor + 4] != b"PK\x03\x04" {
                break;
            }
            let size =
                u32::from_le_bytes(bytes[cursor + 18..cursor + 22].try_into().expect("4 bytes"))
                    as usize;
            let name_length = u16::from_le_bytes([bytes[cursor + 26], bytes[cursor + 27]]) as usize;
            let extra = u16::from_le_bytes([bytes[cursor + 28], bytes[cursor + 29]]) as usize;
            let start = cursor + 30 + name_length + extra;
            let found = String::from_utf8_lossy(&bytes[cursor + 30..cursor + 30 + name_length]);
            if found == name {
                return String::from_utf8_lossy(&bytes[start..start + size]).to_string();
            }
            cursor = start + size;
        }
        String::new()
    }

    /// Every part in the archive, in the order written.
    pub fn parts(bytes: &[u8]) -> Vec<String> {
        let mut names = Vec::new();
        let mut cursor = 0usize;
        while cursor + 30 <= bytes.len() && &bytes[cursor..cursor + 4] == b"PK\x03\x04" {
            let size =
                u32::from_le_bytes(bytes[cursor + 18..cursor + 22].try_into().expect("4 bytes"))
                    as usize;
            let name_length = u16::from_le_bytes([bytes[cursor + 26], bytes[cursor + 27]]) as usize;
            let extra = u16::from_le_bytes([bytes[cursor + 28], bytes[cursor + 29]]) as usize;
            names.push(
                String::from_utf8_lossy(&bytes[cursor + 30..cursor + 30 + name_length]).to_string(),
            );
            cursor += 30 + name_length + extra + size;
        }
        names
    }

    /// Resolve a relationship `Target` against the part its `.rels` belongs to.
    ///
    /// `word/_rels/document.xml.rels` describes `word/document.xml`, so a
    /// target of `styles.xml` means `word/styles.xml` and `../theme/theme1.xml`
    /// means `theme/theme1.xml`. Getting this wrong is how a part that exists
    /// still ends up unreachable.
    fn resolve(rels: &str, target: &str) -> String {
        let base = rels
            .rsplit_once("/_rels/")
            .map(|(before, _)| before)
            .unwrap_or("");
        let mut segments: Vec<&str> = if base.is_empty() {
            Vec::new()
        } else {
            base.split('/').collect()
        };
        for segment in target.split('/') {
            match segment {
                "." | "" => {}
                ".." => {
                    segments.pop();
                }
                other => segments.push(other),
            }
        }
        segments.join("/")
    }

    /// Assert an archive is one an Office application will accept.
    ///
    /// Not "the parts we meant to write are there" — the per-format tests do
    /// that — but the two structural properties that make Word, Excel and
    /// PowerPoint offer to *repair* a file instead of opening it: a
    /// relationship pointing at a part that is not in the archive, and a part
    /// with no declared content type.
    pub fn assert_well_formed_package(bytes: &[u8]) {
        let names = parts(bytes);
        assert!(!names.is_empty(), "the archive has no parts at all");

        let types = part(bytes, "[Content_Types].xml");
        assert!(!types.is_empty(), "there is no [Content_Types].xml");

        for name in &names {
            if name == "[Content_Types].xml" {
                continue;
            }
            let extension = name.rsplit('.').next().unwrap_or("");
            let covered = types.contains(&format!(r#"PartName="/{name}""#))
                || types.contains(&format!(r#"Extension="{extension}""#));
            assert!(
                covered,
                "{name} has no content type, so the package is invalid"
            );
        }

        for rels in names.iter().filter(|name| name.ends_with(".rels")) {
            let body = part(bytes, rels);
            for target in body.split(r#"Target=""#).skip(1) {
                let target = target.split('"').next().unwrap_or("");
                if target.starts_with("http") {
                    continue;
                }
                let resolved = resolve(rels, target);
                assert!(
                    names.contains(&resolved),
                    "{rels} points at {target} ({resolved}), which is not in the archive"
                );
            }
        }
    }

    #[test]
    fn a_relationship_target_resolves_against_the_part_it_describes() {
        assert_eq!(
            resolve("word/_rels/document.xml.rels", "styles.xml"),
            "word/styles.xml"
        );
        assert_eq!(
            resolve(
                "ppt/slides/_rels/slide1.xml.rels",
                "../slideLayouts/slideLayout1.xml"
            ),
            "ppt/slideLayouts/slideLayout1.xml"
        );
        assert_eq!(
            resolve("_rels/.rels", "word/document.xml"),
            "word/document.xml"
        );
    }

    #[test]
    fn every_format_produces_a_package_an_office_application_will_open() {
        // The failure this catches is not a missing part — each format tests
        // for those — but a dangling relationship or an undeclared content
        // type, which is what makes Word offer to repair a file rather than
        // open it, and which no amount of correct XML inside prevents.
        let blocks = super::markup::parse("# Heading\n\nText.\n\n| a |\n|---|\n| 1 |");
        assert_well_formed_package(&super::docx::write(Some("A Report"), &blocks));

        let mut sheet = super::xlsx::Sheet::new("Data");
        sheet.rows.push(vec![super::xlsx::Cell::Text("x".into())]);
        assert_well_formed_package(&super::xlsx::write(&[sheet]));

        assert_well_formed_package(&super::pptx::write(&[
            super::pptx::Slide {
                title: "With notes".into(),
                bullets: vec![super::pptx::Bullet::new(0, "a point")],
                notes: Some("and notes, which add two more parts".into()),
            },
            super::pptx::Slide::titled("Without"),
        ]));
    }

    #[test]
    fn a_path_must_carry_the_extension_of_what_is_written_to_it() {
        assert!(super::check_extension("report.docx", "docx").is_ok());
        assert!(super::check_extension("a/b/REPORT.DOCX", "docx").is_ok());

        let complaint = super::check_extension("out/report.txt", "docx").expect_err("refused");
        assert!(complaint.contains("out/report.docx"), "{complaint}");
    }

    #[test]
    fn a_path_with_no_extension_is_told_to_add_one_not_to_replace_nothing() {
        // The suggestion used to come out as a bare ".pdf", which is a hidden
        // file with no name and worse than the path it was correcting.
        let missing = super::check_extension("out/report", "pdf").expect_err("refused");
        assert!(missing.contains("out/report.pdf"), "{missing}");
    }

    #[test]
    fn a_dot_in_a_directory_name_is_not_an_extension() {
        let complaint = super::check_extension("q3.reports/summary", "xlsx").expect_err("refused");
        assert!(complaint.contains("q3.reports/summary.xlsx"), "{complaint}");
    }
}
