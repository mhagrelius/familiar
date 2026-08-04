//! Writing a spreadsheet.
//!
//! The thing that separates a spreadsheet from a table of text is that the
//! numbers are numbers. A cell written as an inline string looks identical on
//! screen and cannot be summed, sorted numerically, charted or formatted — and
//! that is the whole reason somebody asked for `.xlsx` rather than `.csv`. So
//! [`Cell`] distinguishes them, and [`Cell::infer`] does the guessing once,
//! where it can be tested, rather than in each caller.
//!
//! Strings go in a shared-strings table because Excel expects one and because a
//! column of repeated categories is the normal case.

use std::collections::HashMap;

use super::xml::escape;
use super::zip::Archive;

/// What is in one cell.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Empty,
    Text(String),
    Number(f64),
    /// `TRUE`/`FALSE`, which Excel stores as a boolean rather than as text.
    Bool(bool),
    /// A formula, without its leading `=`. The value is left for Excel to
    /// compute on open — we do not evaluate anything.
    Formula(String),
}

impl Cell {
    /// Read a cell from text the way a person typing it would mean it.
    ///
    /// Numbers become numbers, `=…` becomes a formula, `true`/`false` become
    /// booleans, and everything else is text. The awkward cases are the ones
    /// that decide the design: `007` and `+44 7700 900123` are *text*, because
    /// storing them as numbers destroys them, and that is a data loss no
    /// formatting can undo.
    pub fn infer(text: &str) -> Self {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Self::Empty;
        }
        if let Some(formula) = trimmed.strip_prefix('=') {
            if !formula.is_empty() {
                return Self::Formula(formula.to_string());
            }
        }
        match trimmed.to_ascii_lowercase().as_str() {
            "true" => return Self::Bool(true),
            "false" => return Self::Bool(false),
            _ => {}
        }

        // A leading zero is significant — an order number, a postcode, a part
        // code — so `0042` stays text while `0` and `0.5` do not.
        let leading_zero =
            trimmed.len() > 1 && trimmed.starts_with('0') && !trimmed.starts_with("0.");
        if leading_zero {
            return Self::Text(trimmed.to_string());
        }

        // Thousands separators and a currency or percent sign are how people
        // write numbers, and Excel should get the number.
        let cleaned: String = trimmed
            .chars()
            .filter(|c| !matches!(c, ',' | '$' | '£' | '€' | ' ' | '\u{a0}'))
            .collect();
        if let Some(percent) = cleaned.strip_suffix('%') {
            if let Ok(number) = percent.parse::<f64>() {
                return Self::Number(number / 100.0);
            }
        }
        // `parse` accepts "inf" and "NaN", which are words far more often than
        // they are numbers, and neither has an XLSX representation.
        if cleaned.chars().any(|c| c.is_ascii_digit()) {
            if let Ok(number) = cleaned.parse::<f64>() {
                if number.is_finite() {
                    return Self::Number(number);
                }
            }
        }
        Self::Text(trimmed.to_string())
    }
}

/// One sheet: a name, and rows of cells.
#[derive(Debug, Clone, Default)]
pub struct Sheet {
    pub name: String,
    /// The first row is styled as a header when `header` is set.
    pub header: bool,
    pub rows: Vec<Vec<Cell>>,
}

impl Sheet {
    pub fn new(name: &str) -> Self {
        Self {
            name: sanitise(name),
            header: true,
            rows: Vec::new(),
        }
    }

    /// The widest row, which is how many columns the sheet has.
    fn columns(&self) -> usize {
        self.rows.iter().map(Vec::len).max().unwrap_or(0)
    }
}

/// Excel refuses a sheet name with `[]:*?/\` in it, one over 31 characters, or
/// an empty one — and refuses the *file*, not the name, so it is fixed here.
fn sanitise(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|c| if "[]:*?/\\".contains(c) { '-' } else { c })
        .collect();
    let cleaned = cleaned.trim_matches('\'').trim();
    if cleaned.is_empty() {
        return "Sheet1".to_string();
    }
    cleaned.chars().take(31).collect()
}

/// Build an `.xlsx` from one or more sheets.
///
/// A workbook with no sheets is not a workbook Excel will open, so an empty
/// list produces one empty sheet rather than a broken file.
pub fn write(sheets: &[Sheet]) -> Vec<u8> {
    let fallback = [Sheet::new("Sheet1")];
    let sheets = if sheets.is_empty() {
        &fallback[..]
    } else {
        sheets
    };

    // Names have to be unique as well as legal, or Excel repairs the file.
    let mut seen: HashMap<String, usize> = HashMap::new();
    let sheets: Vec<Sheet> = sheets
        .iter()
        .map(|sheet| {
            let mut sheet = sheet.clone();
            sheet.name = sanitise(&sheet.name);
            let count = seen.entry(sheet.name.to_lowercase()).or_insert(0);
            *count += 1;
            if *count > 1 {
                let suffix = format!(" ({count})");
                let keep = 31 - suffix.len();
                sheet.name = format!(
                    "{}{suffix}",
                    sheet.name.chars().take(keep).collect::<String>()
                );
            }
            sheet
        })
        .collect();

    let mut strings = Strings::default();
    let bodies: Vec<String> = sheets
        .iter()
        .map(|sheet| sheet_xml(sheet, &mut strings))
        .collect();

    let mut archive = Archive::new();
    archive.add_text("[Content_Types].xml", &content_types(sheets.len()));
    archive.add_text("_rels/.rels", ROOT_RELATIONSHIPS);
    archive.add_text(
        "xl/_rels/workbook.xml.rels",
        &workbook_relationships(sheets.len()),
    );
    archive.add_text("xl/workbook.xml", &workbook(&sheets));
    archive.add_text("xl/styles.xml", STYLES);
    archive.add_text("xl/sharedStrings.xml", &strings.finish());
    for (index, body) in bodies.iter().enumerate() {
        archive.add_text(&format!("xl/worksheets/sheet{}.xml", index + 1), body);
    }
    archive.finish()
}

/// The shared-strings table, and the index of each string in it.
#[derive(Default)]
struct Strings {
    order: Vec<String>,
    index: HashMap<String, usize>,
    /// Every use, including repeats — Excel wants both counts.
    total: usize,
}

impl Strings {
    fn intern(&mut self, text: &str) -> usize {
        self.total += 1;
        if let Some(found) = self.index.get(text) {
            return *found;
        }
        let position = self.order.len();
        self.order.push(text.to_string());
        self.index.insert(text.to_string(), position);
        position
    }

    fn finish(&self) -> String {
        let items: String = self
            .order
            .iter()
            .map(|text| format!("<si><t xml:space=\"preserve\">{}</t></si>", escape(text)))
            .collect();
        format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<sst xmlns="{SPREADSHEET}" count="{}" uniqueCount="{}">{items}</sst>"#,
            self.total,
            self.order.len()
        )
    }
}

fn sheet_xml(sheet: &Sheet, strings: &mut Strings) -> String {
    let columns = sheet.columns();
    let mut rows = String::new();

    for (index, cells) in sheet.rows.iter().enumerate() {
        let number = index + 1;
        let header = sheet.header && index == 0;
        let mut row = format!("<row r=\"{number}\">");
        for (column, cell) in cells.iter().enumerate() {
            row.push_str(&cell_xml(cell, column, number, header, strings));
        }
        row.push_str("</row>");
        rows.push_str(&row);
    }

    // Column widths from the longest value in each, which is the difference
    // between a readable sheet and a row of `####`.
    let mut columns_xml = String::new();
    if columns > 0 {
        columns_xml.push_str("<cols>");
        for column in 0..columns {
            let widest = sheet
                .rows
                .iter()
                .filter_map(|row| row.get(column))
                .map(display_width)
                .max()
                .unwrap_or(8);
            let width = (widest + 2).clamp(8, 60);
            columns_xml.push_str(&format!(
                r#"<col min="{}" max="{}" width="{width}" customWidth="1"/>"#,
                column + 1,
                column + 1
            ));
        }
        columns_xml.push_str("</cols>");
    }

    // A frozen header row and an auto-filter, because a spreadsheet somebody
    // has to read is a spreadsheet they will scroll and sort.
    let (freeze, filter) = if sheet.header && !sheet.rows.is_empty() {
        (
            r#"<pane ySplit="1" topLeftCell="A2" activePane="bottomLeft" state="frozen"/>"#,
            format!(
                r#"<autoFilter ref="A1:{}{}"/>"#,
                column_name(columns.max(1) - 1),
                sheet.rows.len()
            ),
        )
    } else {
        ("", String::new())
    };

    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="{SPREADSHEET}"><sheetViews><sheetView workbookViewId="0">{freeze}</sheetView></sheetViews>{columns_xml}<sheetData>{rows}</sheetData>{filter}</worksheet>"#
    )
}

fn cell_xml(cell: &Cell, column: usize, row: usize, header: bool, strings: &mut Strings) -> String {
    let reference = format!("{}{row}", column_name(column));
    // Style 1 is the header; 0 is the default. Both are defined in `STYLES`.
    let style = if header { r#" s="1""# } else { "" };

    match cell {
        Cell::Empty if header => format!(r#"<c r="{reference}"{style}/>"#),
        Cell::Empty => String::new(),
        Cell::Number(number) => {
            format!(
                r#"<c r="{reference}"{style}><v>{}</v></c>"#,
                number_text(*number)
            )
        }
        Cell::Bool(value) => format!(
            r#"<c r="{reference}"{style} t="b"><v>{}</v></c>"#,
            u8::from(*value)
        ),
        Cell::Formula(formula) => format!(
            r#"<c r="{reference}"{style}><f>{}</f></c>"#,
            escape(formula)
        ),
        Cell::Text(text) => {
            let index = strings.intern(text);
            format!(r#"<c r="{reference}"{style} t="s"><v>{index}</v></c>"#)
        }
    }
}

/// A number as XLSX wants it: no exponent for ordinary magnitudes, no trailing
/// `.0` on an integer, and enough digits that a round trip does not drift.
fn number_text(number: f64) -> String {
    if number == number.trunc() && number.abs() < 1e15 {
        return format!("{}", number as i64);
    }
    let mut text = format!("{number}");
    if text.contains('e') || text.contains('E') {
        text = format!("{number:.10}");
        while text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
    }
    text
}

fn display_width(cell: &Cell) -> usize {
    match cell {
        Cell::Empty => 0,
        Cell::Text(text) => text.chars().count(),
        Cell::Number(number) => number_text(*number).len(),
        Cell::Bool(_) => 5,
        Cell::Formula(formula) => formula.chars().count().min(20),
    }
}

/// 0 → A, 25 → Z, 26 → AA. Excel's column names are bijective base-26, which is
/// not the same as base-26 and is the usual place this goes wrong.
fn column_name(mut index: usize) -> String {
    let mut name = Vec::new();
    loop {
        name.push(b'A' + (index % 26) as u8);
        if index < 26 {
            break;
        }
        index = index / 26 - 1;
    }
    name.reverse();
    String::from_utf8(name).unwrap_or_else(|_| "A".into())
}

fn workbook(sheets: &[Sheet]) -> String {
    let entries: String = sheets
        .iter()
        .enumerate()
        .map(|(index, sheet)| {
            format!(
                r#"<sheet name="{}" sheetId="{}" r:id="rId{}"/>"#,
                escape(&sheet.name),
                index + 1,
                index + 1
            )
        })
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="{SPREADSHEET}" xmlns:r="{RELATIONSHIPS}"><sheets>{entries}</sheets></workbook>"#
    )
}

fn workbook_relationships(count: usize) -> String {
    let mut entries = String::new();
    for index in 0..count {
        entries.push_str(&format!(
            r#"<Relationship Id="rId{}" Type="{OFFICE}/worksheet" Target="worksheets/sheet{}.xml"/>"#,
            index + 1,
            index + 1
        ));
    }
    entries.push_str(&format!(
        r#"<Relationship Id="rId{}" Type="{OFFICE}/styles" Target="styles.xml"/>"#,
        count + 1
    ));
    entries.push_str(&format!(
        r#"<Relationship Id="rId{}" Type="{OFFICE}/sharedStrings" Target="sharedStrings.xml"/>"#,
        count + 2
    ));
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="{PACKAGE}">{entries}</Relationships>"#
    )
}

fn content_types(count: usize) -> String {
    let mut overrides = String::from(
        r#"<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/><Override PartName="/xl/sharedStrings.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml"/>"#,
    );
    for index in 0..count {
        overrides.push_str(&format!(
            r#"<Override PartName="/xl/worksheets/sheet{}.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>"#,
            index + 1
        ));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/>{overrides}</Types>"#
    )
}

const SPREADSHEET: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const RELATIONSHIPS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const OFFICE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const PACKAGE: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

const ROOT_RELATIONSHIPS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#;

/// Two cell formats: 0 is the default, 1 is the header — bold, white on the
/// same blue-grey the Word headings use, so a document and a workbook from the
/// same conversation look related.
const STYLES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><fonts count="2"><font><sz val="11"/><name val="Calibri"/></font><font><b/><sz val="11"/><color rgb="FFFFFFFF"/><name val="Calibri"/></font></fonts><fills count="3"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill><fill><patternFill patternType="solid"><fgColor rgb="FF2E5496"/><bgColor indexed="64"/></patternFill></fill></fills><borders count="1"><border><left/><right/><top/><bottom/><diagonal/></border></borders><cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs><cellXfs count="2"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/><xf numFmtId="0" fontId="1" fillId="2" borderId="0" xfId="0" applyFont="1" applyFill="1" applyAlignment="1"><alignment vertical="center"/></xf></cellXfs></styleSheet>"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn part(bytes: &[u8], name: &str) -> String {
        crate::model::office::tests::part(bytes, name)
    }

    fn sheet_of(rows: &[&[&str]]) -> Sheet {
        let mut sheet = Sheet::new("Data");
        sheet.rows = rows
            .iter()
            .map(|row| row.iter().map(|text| Cell::infer(text)).collect())
            .collect();
        sheet
    }

    #[test]
    fn numbers_are_numbers_and_words_are_words() {
        // The whole reason to ask for xlsx rather than csv: a number stored as
        // text cannot be summed, sorted or charted, and looks identical.
        assert_eq!(Cell::infer("42"), Cell::Number(42.0));
        assert_eq!(Cell::infer("-3.5"), Cell::Number(-3.5));
        assert_eq!(Cell::infer("1,234"), Cell::Number(1234.0));
        assert_eq!(Cell::infer("£1,250.50"), Cell::Number(1250.5));
        assert_eq!(Cell::infer("12%"), Cell::Number(0.12));
        assert_eq!(Cell::infer("Revenue"), Cell::Text("Revenue".into()));
    }

    #[test]
    fn a_leading_zero_is_kept_as_text() {
        // An order number, a postcode or a part code turned into a number is
        // data loss no formatting can undo.
        assert_eq!(Cell::infer("007"), Cell::Text("007".into()));
        assert_eq!(Cell::infer("0"), Cell::Number(0.0));
        assert_eq!(Cell::infer("0.5"), Cell::Number(0.5));
    }

    #[test]
    fn words_that_parse_as_numbers_are_still_words() {
        // `"NaN".parse::<f64>()` succeeds, and a column of test outcomes that
        // says NaN means the word.
        assert_eq!(Cell::infer("NaN"), Cell::Text("NaN".into()));
        assert_eq!(Cell::infer("inf"), Cell::Text("inf".into()));
        assert_eq!(Cell::infer("infinity"), Cell::Text("infinity".into()));
    }

    #[test]
    fn formulas_and_booleans_are_told_apart_from_text() {
        assert_eq!(
            Cell::infer("=SUM(B2:B10)"),
            Cell::Formula("SUM(B2:B10)".into())
        );
        assert_eq!(Cell::infer("TRUE"), Cell::Bool(true));
        assert_eq!(Cell::infer("false"), Cell::Bool(false));
        assert_eq!(Cell::infer("="), Cell::Text("=".into()));
        assert_eq!(Cell::infer(""), Cell::Empty);
    }

    #[test]
    fn columns_are_named_in_bijective_base_twenty_six() {
        // The usual bug: 26 becomes "BA" or "A@" rather than "AA".
        assert_eq!(column_name(0), "A");
        assert_eq!(column_name(25), "Z");
        assert_eq!(column_name(26), "AA");
        assert_eq!(column_name(51), "AZ");
        assert_eq!(column_name(52), "BA");
        assert_eq!(column_name(701), "ZZ");
        assert_eq!(column_name(702), "AAA");
    }

    #[test]
    fn a_workbook_holds_every_part_excel_needs() {
        let bytes = write(&[sheet_of(&[&["Name", "Count"], &["a", "1"]])]);
        for required in [
            "[Content_Types].xml",
            "_rels/.rels",
            "xl/_rels/workbook.xml.rels",
            "xl/workbook.xml",
            "xl/styles.xml",
            "xl/sharedStrings.xml",
            "xl/worksheets/sheet1.xml",
        ] {
            assert!(!part(&bytes, required).is_empty(), "{required} is missing");
        }
    }

    #[test]
    fn a_number_cell_carries_a_value_and_a_text_cell_an_index() {
        let bytes = write(&[sheet_of(&[&["Name", "Count"], &["a", "42"]])]);
        let sheet = part(&bytes, "xl/worksheets/sheet1.xml");
        assert!(sheet.contains(r#"<c r="B2"><v>42</v></c>"#), "{sheet}");
        assert!(
            sheet.contains(r#"t="s""#),
            "text goes through shared strings"
        );
    }

    #[test]
    fn a_repeated_string_is_stored_once() {
        let bytes = write(&[sheet_of(&[&["x"], &["same"], &["same"], &["same"]])]);
        let strings = part(&bytes, "xl/sharedStrings.xml");
        assert!(strings.contains(r#"count="4""#), "{strings}");
        assert!(strings.contains(r#"uniqueCount="2""#), "{strings}");
        assert_eq!(strings.matches("<si>").count(), 2);
    }

    #[test]
    fn the_header_row_is_styled_frozen_and_filterable() {
        let bytes = write(&[sheet_of(&[&["Name", "Count"], &["a", "1"]])]);
        let sheet = part(&bytes, "xl/worksheets/sheet1.xml");
        assert!(
            sheet.contains(r#"s="1""#),
            "header cells are styled: {sheet}"
        );
        assert!(sheet.contains(r#"state="frozen""#), "{sheet}");
        assert!(sheet.contains(r#"<autoFilter ref="A1:B2"/>"#), "{sheet}");
    }

    #[test]
    fn a_sheet_without_a_header_is_neither_frozen_nor_filtered() {
        let mut sheet = sheet_of(&[&["a", "1"]]);
        sheet.header = false;
        let body = part(&write(&[sheet]), "xl/worksheets/sheet1.xml");
        assert!(!body.contains("frozen"), "{body}");
        assert!(!body.contains("autoFilter"), "{body}");
    }

    #[test]
    fn sheet_names_excel_would_refuse_are_fixed_rather_than_passed_on() {
        // Excel rejects the whole file, not just the name, so this cannot be
        // left to the caller.
        assert_eq!(sanitise("Q1/Q2 [draft]"), "Q1-Q2 -draft-");
        assert_eq!(sanitise(""), "Sheet1");
        assert_eq!(sanitise(&"x".repeat(40)).chars().count(), 31);
    }

    #[test]
    fn two_sheets_of_the_same_name_get_different_ones() {
        // Duplicate names make Excel announce that the file needs repair.
        let bytes = write(&[Sheet::new("Data"), Sheet::new("Data")]);
        let workbook = part(&bytes, "xl/workbook.xml");
        assert!(workbook.contains(r#"name="Data""#), "{workbook}");
        assert!(workbook.contains(r#"name="Data (2)""#), "{workbook}");
    }

    #[test]
    fn every_sheet_gets_its_own_part_and_relationship() {
        let bytes = write(&[Sheet::new("One"), Sheet::new("Two"), Sheet::new("Three")]);
        assert!(!part(&bytes, "xl/worksheets/sheet3.xml").is_empty());
        let relationships = part(&bytes, "xl/_rels/workbook.xml.rels");
        assert!(
            relationships.contains("worksheets/sheet3.xml"),
            "{relationships}"
        );
        assert!(part(&bytes, "[Content_Types].xml").contains("/xl/worksheets/sheet3.xml"));
    }

    #[test]
    fn a_workbook_with_no_sheets_still_opens() {
        let bytes = write(&[]);
        assert!(
            part(&bytes, "xl/workbook.xml").contains("<sheet "),
            "one sheet is made"
        );
    }

    #[test]
    fn text_that_looks_like_xml_cannot_break_the_workbook() {
        let bytes = write(&[sheet_of(&[&["a & b <c>"]])]);
        let strings = part(&bytes, "xl/sharedStrings.xml");
        assert!(strings.contains("a &amp; b &lt;c&gt;"), "{strings}");
        assert_eq!(strings.matches("</sst>").count(), 1);
    }

    #[test]
    fn a_whole_number_is_not_written_with_a_trailing_zero() {
        // `<v>42.0</v>` is legal and makes every integer column look like a
        // decimal one once Excel picks a format from the data.
        assert_eq!(number_text(42.0), "42");
        assert_eq!(number_text(-7.0), "-7");
        assert_eq!(number_text(0.5), "0.5");
        assert!(!number_text(0.000_001).contains('e'), "no exponent form");
    }

    #[test]
    fn a_ragged_row_does_not_shift_the_cells_after_it() {
        // Cells carry their own reference, so a short row leaves a gap rather
        // than sliding the next row's values into the wrong column.
        let bytes = write(&[sheet_of(&[&["a", "b", "c"], &["1"], &["1", "2", "3"]])]);
        let sheet = part(&bytes, "xl/worksheets/sheet1.xml");
        assert!(sheet.contains(r#"<c r="C3">"#), "{sheet}");
    }
}
