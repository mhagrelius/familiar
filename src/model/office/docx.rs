//! Writing a Word document.
//!
//! A `.docx` is a ZIP of XML parts, and the minimum Word will open is smaller
//! than its reputation: a content-type map, two relationship parts, the
//! document body, a style sheet and a numbering definition. That is what this
//! writes — no `settings.xml`, no `fontTable.xml`, no `theme1.xml`, because
//! Word supplies its own defaults for every one of them and a part we do not
//! understand is a part that can be wrong.
//!
//! Styles are named rather than inline. `Heading1` on a paragraph is what makes
//! Word's navigation pane work, what makes "update table of contents" find
//! anything, and what makes the document restyle when the user picks a theme —
//! all of which a hand-set 18pt bold run loses.

use super::markup::{Block, Span};
use super::xml::escape;
use super::zip::Archive;

/// A twip is a twentieth of a point, which is the unit Word measures in.
const fn half_points(points: u32) -> u32 {
    points * 2
}

/// The one number that decides how a list looks: how far each level indents.
const INDENT_PER_LEVEL: u32 = 360;

/// Build a `.docx` from a title and a body of blocks.
///
/// `title` is the document's metadata title and, when there is one, its first
/// heading — a Word file whose properties say "Untitled" is a small
/// indignity that costs one line to avoid.
pub fn write(title: Option<&str>, blocks: &[Block]) -> Vec<u8> {
    let mut archive = Archive::new();
    archive.add_text("[Content_Types].xml", CONTENT_TYPES);
    archive.add_text("_rels/.rels", ROOT_RELATIONSHIPS);
    archive.add_text("word/_rels/document.xml.rels", DOCUMENT_RELATIONSHIPS);
    archive.add_text("word/styles.xml", STYLES);
    archive.add_text("word/numbering.xml", NUMBERING);
    archive.add_text("docProps/core.xml", &core_properties(title));
    archive.add_text("word/document.xml", &document(title, blocks));
    archive.finish()
}

fn document(title: Option<&str>, blocks: &[Block]) -> String {
    let mut body = String::new();

    if let Some(title) = title.filter(|title| !title.trim().is_empty()) {
        body.push_str(&paragraph("Title", &[Span::plain(title)], None));
    }

    for block in blocks {
        body.push_str(&render(block));
    }

    // Word wants a section properties element last; without it the document
    // opens but has no page size and prints on whatever the driver guesses.
    // A4 with one-inch margins, in twips.
    format!(
        concat!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
            r#"<w:document xmlns:w="{}"><w:body>{}"#,
            r#"<w:sectPr><w:pgSz w:w="11906" w:h="16838"/>"#,
            r#"<w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440""#,
            r#" w:header="708" w:footer="708" w:gutter="0"/></w:sectPr>"#,
            "</w:body></w:document>"
        ),
        W, body
    )
}

fn render(block: &Block) -> String {
    match block {
        Block::Heading { level, spans } => {
            paragraph(&format!("Heading{}", (*level).clamp(1, 6)), spans, None)
        }
        Block::Paragraph { spans } => paragraph("Normal", spans, None),
        Block::Quote { spans } => paragraph("Quote", spans, None),
        Block::Bullet { level, spans } => paragraph("ListParagraph", spans, Some((1, *level))),
        Block::Numbered { level, spans } => paragraph("ListParagraph", spans, Some((2, *level))),
        Block::Rule => {
            // A rule is a paragraph with a bottom border, which is how Word
            // itself writes one — there is no horizontal-rule element.
            r#"<w:p><w:pPr><w:pBdr><w:bottom w:val="single" w:sz="6" w:space="1" w:color="BFBFBF"/></w:pBdr></w:pPr></w:p>"#.to_string()
        }
        Block::PageBreak => r#"<w:p><w:r><w:br w:type="page"/></w:r></w:p>"#.to_string(),
        Block::Code { text, .. } => {
            // One paragraph per line, each shaded: a code block with soft line
            // breaks inside a single paragraph loses its indentation the first
            // time anybody reflows it.
            text.lines()
                .map(|line| {
                    paragraph(
                        "Code",
                        &[Span {
                            text: line.to_string(),
                            code: true,
                            ..Default::default()
                        }],
                        None,
                    )
                })
                .collect()
        }
        Block::Table { header, rows } => table(header, rows),
    }
}

/// One paragraph: a style, optional list membership, and its runs.
fn paragraph(style: &str, spans: &[Span], numbering: Option<(u32, u8)>) -> String {
    let mut properties = format!(r#"<w:pStyle w:val="{style}"/>"#);
    if let Some((list, level)) = numbering {
        let level = level.min(4) as u32;
        properties.push_str(&format!(
            r#"<w:numPr><w:ilvl w:val="{level}"/><w:numId w:val="{list}"/></w:numPr>"#
        ));
        properties.push_str(&format!(
            r#"<w:ind w:left="{}" w:hanging="{INDENT_PER_LEVEL}"/>"#,
            (level + 1) * INDENT_PER_LEVEL
        ));
    }

    let runs: String = spans.iter().map(run).collect();
    format!("<w:p><w:pPr>{properties}</w:pPr>{runs}</w:p>")
}

fn run(span: &Span) -> String {
    let mut properties = String::new();
    if span.bold {
        properties.push_str("<w:b/>");
    }
    if span.italic {
        properties.push_str("<w:i/>");
    }
    if span.code {
        properties.push_str(r#"<w:rFonts w:ascii="Consolas" w:hAnsi="Consolas" w:cs="Consolas"/>"#);
        properties.push_str(&format!(r#"<w:sz w:val="{}"/>"#, half_points(10)));
    }
    if span.link.is_some() {
        // Styled as a link rather than made one: a real hyperlink needs a
        // relationship id per target, and a document full of them is a document
        // full of ways for the two to fall out of step. The URL is kept in the
        // text so nothing is lost.
        properties.push_str(r#"<w:color w:val="0563C1"/><w:u w:val="single"/>"#);
    }

    let text = match &span.link {
        Some(url) if url != &span.text => format!("{} ({url})", span.text),
        _ => span.text.clone(),
    };
    // xml:space="preserve" or Word eats the space between two runs, which is
    // exactly what "plain **bold**" is made of.
    format!(
        r#"<w:r><w:rPr>{properties}</w:rPr><w:t xml:space="preserve">{}</w:t></w:r>"#,
        escape(&text)
    )
}

fn table(header: &[String], rows: &[Vec<String>]) -> String {
    let columns = header
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    if columns == 0 {
        return String::new();
    }
    // A 9026-twip content width divided evenly. Word will redistribute on
    // first edit; this only has to be sane on open.
    let width = 9026 / columns as u32;

    let mut out = String::from(concat!(
        r#"<w:tbl><w:tblPr><w:tblStyle w:val="TableGrid"/><w:tblW w:w="0" w:type="auto"/>"#,
        r#"<w:tblBorders>"#,
        r#"<w:top w:val="single" w:sz="4" w:space="0" w:color="BFBFBF"/>"#,
        r#"<w:left w:val="single" w:sz="4" w:space="0" w:color="BFBFBF"/>"#,
        r#"<w:bottom w:val="single" w:sz="4" w:space="0" w:color="BFBFBF"/>"#,
        r#"<w:right w:val="single" w:sz="4" w:space="0" w:color="BFBFBF"/>"#,
        r#"<w:insideH w:val="single" w:sz="4" w:space="0" w:color="BFBFBF"/>"#,
        r#"<w:insideV w:val="single" w:sz="4" w:space="0" w:color="BFBFBF"/>"#,
        r#"</w:tblBorders></w:tblPr><w:tblGrid>"#,
    ));
    for _ in 0..columns {
        out.push_str(&format!(r#"<w:gridCol w:w="{width}"/>"#));
    }
    out.push_str("</w:tblGrid>");

    if !header.is_empty() {
        out.push_str(&row(header, columns, width, true));
    }
    for cells in rows {
        out.push_str(&row(cells, columns, width, false));
    }
    out.push_str("</w:tbl>");
    // A table immediately followed by another table merges into one in Word.
    // An empty paragraph after it is the standard separator.
    out.push_str(r#"<w:p><w:pPr><w:pStyle w:val="Normal"/></w:pPr></w:p>"#);
    out
}

fn row(cells: &[String], columns: usize, width: u32, header: bool) -> String {
    let mut out = String::from("<w:tr>");
    if header {
        // Repeat on every page, which is what a person means by a header row.
        out.push_str(r#"<w:trPr><w:tblHeader/></w:trPr>"#);
    }
    for column in 0..columns {
        let text = cells.get(column).map(String::as_str).unwrap_or("");
        let spans = super::markup::spans(text);
        let spans: Vec<Span> = spans
            .into_iter()
            .map(|mut span| {
                span.bold = span.bold || header;
                span
            })
            .collect();
        out.push_str(&format!(
            r#"<w:tc><w:tcPr><w:tcW w:w="{width}" w:type="dxa"/>{}</w:tcPr>{}</w:tc>"#,
            if header {
                r#"<w:shd w:val="clear" w:color="auto" w:fill="F2F2F2"/>"#
            } else {
                ""
            },
            paragraph("TableText", &spans, None)
        ));
    }
    out.push_str("</w:tr>");
    out
}

fn core_properties(title: Option<&str>) -> String {
    format!(
        concat!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
            r#"<cp:coreProperties"#,
            r#" xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties""#,
            r#" xmlns:dc="http://purl.org/dc/elements/1.1/">"#,
            r#"<dc:title>{}</dc:title><dc:creator>Familiar</dc:creator>"#,
            r#"<cp:lastModifiedBy>Familiar</cp:lastModifiedBy></cp:coreProperties>"#,
        ),
        escape(title.unwrap_or("Untitled"))
    )
}

const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";

const CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/><Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/><Override PartName="/word/numbering.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml"/><Override PartName="/docProps/core.xml" ContentType="application/vnd.openxmlformats-package.core-properties+xml"/></Types>"#;

const ROOT_RELATIONSHIPS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" Target="docProps/core.xml"/></Relationships>"#;

const DOCUMENT_RELATIONSHIPS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering" Target="numbering.xml"/></Relationships>"#;

/// The style sheet. Sizes are half-points, spacing is twips.
///
/// Heading colours are the Office blue-grey rather than black, which is what
/// makes a generated document look like a document rather than like output.
const STYLES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii="Calibri" w:hAnsi="Calibri" w:cs="Calibri"/><w:sz w:val="22"/></w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="160" w:line="259" w:lineRule="auto"/></w:pPr></w:pPrDefault></w:docDefaults><w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/><w:qFormat/></w:style><w:style w:type="paragraph" w:styleId="Title"><w:name w:val="Title"/><w:basedOn w:val="Normal"/><w:qFormat/><w:pPr><w:spacing w:before="0" w:after="320"/></w:pPr><w:rPr><w:sz w:val="56"/><w:color w:val="1F3864"/></w:rPr></w:style><w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/><w:basedOn w:val="Normal"/><w:qFormat/><w:pPr><w:outlineLvl w:val="0"/><w:spacing w:before="360" w:after="120"/><w:keepNext/></w:pPr><w:rPr><w:b/><w:sz w:val="36"/><w:color w:val="1F3864"/></w:rPr></w:style><w:style w:type="paragraph" w:styleId="Heading2"><w:name w:val="heading 2"/><w:basedOn w:val="Normal"/><w:qFormat/><w:pPr><w:outlineLvl w:val="1"/><w:spacing w:before="280" w:after="100"/><w:keepNext/></w:pPr><w:rPr><w:b/><w:sz w:val="30"/><w:color w:val="2E5496"/></w:rPr></w:style><w:style w:type="paragraph" w:styleId="Heading3"><w:name w:val="heading 3"/><w:basedOn w:val="Normal"/><w:qFormat/><w:pPr><w:outlineLvl w:val="2"/><w:spacing w:before="240" w:after="80"/><w:keepNext/></w:pPr><w:rPr><w:b/><w:sz w:val="26"/><w:color w:val="2E5496"/></w:rPr></w:style><w:style w:type="paragraph" w:styleId="Heading4"><w:name w:val="heading 4"/><w:basedOn w:val="Normal"/><w:qFormat/><w:pPr><w:outlineLvl w:val="3"/><w:spacing w:before="200" w:after="80"/><w:keepNext/></w:pPr><w:rPr><w:b/><w:i/><w:sz w:val="24"/><w:color w:val="2E5496"/></w:rPr></w:style><w:style w:type="paragraph" w:styleId="Heading5"><w:name w:val="heading 5"/><w:basedOn w:val="Heading4"/><w:qFormat/><w:pPr><w:outlineLvl w:val="4"/></w:pPr></w:style><w:style w:type="paragraph" w:styleId="Heading6"><w:name w:val="heading 6"/><w:basedOn w:val="Heading4"/><w:qFormat/><w:pPr><w:outlineLvl w:val="5"/></w:pPr></w:style><w:style w:type="paragraph" w:styleId="ListParagraph"><w:name w:val="List Paragraph"/><w:basedOn w:val="Normal"/><w:qFormat/><w:pPr><w:spacing w:after="60"/><w:contextualSpacing/></w:pPr></w:style><w:style w:type="paragraph" w:styleId="Quote"><w:name w:val="Quote"/><w:basedOn w:val="Normal"/><w:qFormat/><w:pPr><w:ind w:left="720" w:right="720"/><w:spacing w:before="160" w:after="160"/></w:pPr><w:rPr><w:i/><w:color w:val="404040"/></w:rPr></w:style><w:style w:type="paragraph" w:styleId="Code"><w:name w:val="HTML Preformatted"/><w:basedOn w:val="Normal"/><w:qFormat/><w:pPr><w:spacing w:after="0" w:line="240" w:lineRule="auto"/><w:ind w:left="360"/><w:shd w:val="clear" w:color="auto" w:fill="F5F5F5"/></w:pPr><w:rPr><w:rFonts w:ascii="Consolas" w:hAnsi="Consolas" w:cs="Consolas"/><w:sz w:val="20"/></w:rPr></w:style><w:style w:type="paragraph" w:styleId="TableText"><w:name w:val="Table Text"/><w:basedOn w:val="Normal"/><w:pPr><w:spacing w:before="40" w:after="40" w:line="240" w:lineRule="auto"/></w:pPr></w:style><w:style w:type="table" w:styleId="TableGrid"><w:name w:val="Table Grid"/><w:tblPr><w:tblBorders><w:top w:val="single" w:sz="4" w:space="0" w:color="BFBFBF"/><w:left w:val="single" w:sz="4" w:space="0" w:color="BFBFBF"/><w:bottom w:val="single" w:sz="4" w:space="0" w:color="BFBFBF"/><w:right w:val="single" w:sz="4" w:space="0" w:color="BFBFBF"/><w:insideH w:val="single" w:sz="4" w:space="0" w:color="BFBFBF"/><w:insideV w:val="single" w:sz="4" w:space="0" w:color="BFBFBF"/></w:tblBorders></w:tblPr></w:style></w:styles>"#;

/// Two lists: `numId` 1 is bulleted, 2 is numbered, five levels each.
const NUMBERING: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:abstractNum w:abstractNumId="1"><w:multiLevelType w:val="hybridMultilevel"/><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="bullet"/><w:lvlText w:val="•"/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="360" w:hanging="360"/></w:pPr><w:rPr><w:rFonts w:ascii="Symbol" w:hAnsi="Symbol" w:hint="default"/></w:rPr></w:lvl><w:lvl w:ilvl="1"><w:start w:val="1"/><w:numFmt w:val="bullet"/><w:lvlText w:val="◦"/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="720" w:hanging="360"/></w:pPr></w:lvl><w:lvl w:ilvl="2"><w:start w:val="1"/><w:numFmt w:val="bullet"/><w:lvlText w:val="▪"/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="1080" w:hanging="360"/></w:pPr></w:lvl><w:lvl w:ilvl="3"><w:start w:val="1"/><w:numFmt w:val="bullet"/><w:lvlText w:val="•"/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="1440" w:hanging="360"/></w:pPr></w:lvl><w:lvl w:ilvl="4"><w:start w:val="1"/><w:numFmt w:val="bullet"/><w:lvlText w:val="◦"/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="1800" w:hanging="360"/></w:pPr></w:lvl></w:abstractNum><w:abstractNum w:abstractNumId="2"><w:multiLevelType w:val="hybridMultilevel"/><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="360" w:hanging="360"/></w:pPr></w:lvl><w:lvl w:ilvl="1"><w:start w:val="1"/><w:numFmt w:val="lowerLetter"/><w:lvlText w:val="%2."/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="720" w:hanging="360"/></w:pPr></w:lvl><w:lvl w:ilvl="2"><w:start w:val="1"/><w:numFmt w:val="lowerRoman"/><w:lvlText w:val="%3."/><w:lvlJc w:val="right"/><w:pPr><w:ind w:left="1080" w:hanging="180"/></w:pPr></w:lvl><w:lvl w:ilvl="3"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%4."/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="1440" w:hanging="360"/></w:pPr></w:lvl><w:lvl w:ilvl="4"><w:start w:val="1"/><w:numFmt w:val="lowerLetter"/><w:lvlText w:val="%5."/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="1800" w:hanging="360"/></w:pPr></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="1"/></w:num><w:num w:numId="2"><w:abstractNumId w:val="2"/></w:num></w:numbering>"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::office::markup;

    /// Pull one stored part back out of the archive. The entries are stored, so
    /// the bytes are there verbatim and a test can read what Word will read.
    fn part(bytes: &[u8], name: &str) -> String {
        crate::model::office::tests::part(bytes, name)
    }

    fn build(markdown: &str) -> Vec<u8> {
        write(Some("A Report"), &markup::parse(markdown))
    }

    #[test]
    fn the_archive_holds_every_part_word_needs_to_open_it() {
        let bytes = build("Hello.");
        for required in [
            "[Content_Types].xml",
            "_rels/.rels",
            "word/_rels/document.xml.rels",
            "word/document.xml",
            "word/styles.xml",
            "word/numbering.xml",
            "docProps/core.xml",
        ] {
            assert!(!part(&bytes, required).is_empty(), "{required} is missing");
        }
    }

    #[test]
    fn a_docx_is_a_zip() {
        assert_eq!(&build("Hello.")[..2], b"PK");
    }

    #[test]
    fn headings_use_word_styles_rather_than_a_hand_set_size() {
        // The styles are what make the navigation pane and a table of contents
        // work; an 18pt bold run looks the same and does neither.
        let document = part(&build("# Chapter\n\n## Section"), "word/document.xml");
        assert!(
            document.contains(r#"<w:pStyle w:val="Heading1"/>"#),
            "{document}"
        );
        assert!(document.contains(r#"<w:pStyle w:val="Heading2"/>"#));
    }

    #[test]
    fn the_title_becomes_both_metadata_and_the_first_paragraph() {
        let bytes = build("Body text.");
        assert!(part(&bytes, "docProps/core.xml").contains("<dc:title>A Report</dc:title>"));
        assert!(part(&bytes, "word/document.xml").contains(r#"<w:pStyle w:val="Title"/>"#));
    }

    #[test]
    fn bold_and_italic_become_run_properties() {
        let document = part(&build("plain **bold** *slanted*"), "word/document.xml");
        assert!(document.contains("<w:b/>"), "{document}");
        assert!(document.contains("<w:i/>"));
    }

    #[test]
    fn the_space_between_two_runs_survives() {
        // Without xml:space="preserve" Word eats it, and "plain bold" becomes
        // "plainbold" — which is what "plain **bold**" is made of.
        let document = part(&build("plain **bold**"), "word/document.xml");
        assert!(
            document.contains(r#"<w:t xml:space="preserve">plain </w:t>"#),
            "{document}"
        );
    }

    #[test]
    fn bullets_and_numbers_reference_different_lists() {
        let document = part(&build("- one\n\n1. two"), "word/document.xml");
        assert!(document.contains(r#"<w:numId w:val="1"/>"#), "{document}");
        assert!(document.contains(r#"<w:numId w:val="2"/>"#));
    }

    #[test]
    fn a_nested_bullet_indents_by_its_level() {
        let document = part(&build("- top\n  - nested"), "word/document.xml");
        assert!(document.contains(r#"<w:ilvl w:val="1"/>"#), "{document}");
    }

    #[test]
    fn a_table_gets_a_grid_a_header_row_and_a_paragraph_after_it() {
        let document = part(
            &build("| Name | Count |\n|---|---|\n| a | 1 |"),
            "word/document.xml",
        );
        assert!(document.contains("<w:tbl>"), "{document}");
        assert_eq!(document.matches("<w:gridCol").count(), 2);
        assert!(document.contains("<w:tblHeader/>"), "header rows repeat");
        // Two tables in a row merge into one in Word without a separator.
        assert!(document.contains("</w:tbl><w:p>"), "{document}");
    }

    #[test]
    fn a_ragged_table_row_is_padded_rather_than_dropped() {
        // A model writes a short row often enough that losing the cells after
        // it would be a common, silent corruption.
        let document = part(
            &build("| a | b | c |\n|---|---|---|\n| 1 |"),
            "word/document.xml",
        );
        let rows: Vec<&str> = document.split("<w:tr>").skip(1).collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].matches("<w:tc>").count(), 3);
    }

    #[test]
    fn a_code_block_is_one_paragraph_per_line() {
        // Soft breaks inside one paragraph lose their indentation the first
        // time anybody reflows the document.
        let document = part(
            &build("```rust\nlet a = 1;\nlet b = 2;\n```"),
            "word/document.xml",
        );
        assert_eq!(document.matches(r#"<w:pStyle w:val="Code"/>"#).count(), 2);
    }

    #[test]
    fn text_that_looks_like_xml_cannot_break_the_document() {
        let document = part(&build("a < b && c > d, said \"x\""), "word/document.xml");
        assert!(
            document.contains("a &lt; b &amp;&amp; c &gt; d"),
            "{document}"
        );
        // And the body still closes exactly once.
        assert_eq!(document.matches("</w:document>").count(), 1);
    }

    #[test]
    fn a_page_break_is_a_break_run() {
        let document = part(&build("one\n\n\\pagebreak\n\ntwo"), "word/document.xml");
        assert!(document.contains(r#"<w:br w:type="page"/>"#), "{document}");
    }

    #[test]
    fn an_empty_document_still_opens() {
        let document = part(&write(None, &[]), "word/document.xml");
        assert!(document.contains("<w:body>"), "{document}");
        assert!(document.contains("<w:sectPr>"), "a page size is always set");
    }

    #[test]
    fn a_link_keeps_its_target_where_a_reader_can_see_it() {
        let document = part(
            &build("see [the design](https://example.com)"),
            "word/document.xml",
        );
        assert!(
            document.contains("the design (https://example.com)"),
            "{document}"
        );
    }
}
