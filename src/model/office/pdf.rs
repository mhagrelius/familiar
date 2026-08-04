//! Painting a document to PDF.
//!
//! Cairo and Pango are already linked into this process — GTK draws every
//! widget through them — and `cairo::PdfSurface` is a first-class output, so a
//! PDF costs no new program, no second runtime and nothing at packaging time.
//! It also reads the same [`Block`] spec the `.docx` writer does, which is what
//! makes the Word file and the PDF of one document agree; an external converter
//! could only promise that by being the same converter.
//!
//! **Text is placed one line at a time, not one layout at a time.** A Pango
//! layout drawn whole cannot straddle a page boundary, so a paragraph longer
//! than the space left would either overflow the margin or push the whole
//! paragraph to the next page and leave a hole. Walking the layout's lines and
//! starting a page when the next one does not fit is what makes a document of
//! arbitrary length come out right — and it is why [`Cursor`] exists at all.
//!
//! Nothing here needs a display. Pango resolves fonts through fontconfig and
//! Cairo writes to a stream, so this module's tests run in the same
//! display-free suite as the rest of `model/`.

use std::io;

use super::markup::{Block, Span};

/// A4 in points, which is what a PDF measures in.
const PAGE_WIDTH: f64 = 595.276;
const PAGE_HEIGHT: f64 = 841.89;
/// One inch of margin on every side.
const MARGIN: f64 = 72.0;
/// Where the footer sits, measured up from the bottom of the page.
const FOOTER: f64 = 36.0;

const BODY_SIZE: f64 = 10.5;
const CODE_SIZE: f64 = 9.0;
const FOOTER_SIZE: f64 = 8.0;

/// Families, in preference order. Pango falls through the list, so this works
/// on a bare runtime as well as a full desktop.
const SANS: &str = "Cantarell, DejaVu Sans, Liberation Sans, sans-serif";
const MONO: &str = "Source Code Pro, DejaVu Sans Mono, Liberation Mono, monospace";

/// Heading sizes and the space above each, by level.
fn heading_style(level: u8) -> (f64, f64, bool) {
    match level.clamp(1, 6) {
        1 => (20.0, 18.0, true),
        2 => (16.0, 16.0, true),
        3 => (13.0, 14.0, true),
        4 => (11.5, 12.0, true),
        _ => (10.5, 10.0, true),
    }
}

/// Something went wrong that is worth telling the model about.
#[derive(Debug)]
pub enum Error {
    /// Cairo could not make a surface or write to it.
    Surface(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Surface(detail) => write!(f, "the PDF could not be drawn: {detail}"),
        }
    }
}

impl std::error::Error for Error {}

/// Somewhere to put the bytes while Cairo streams them.
///
/// `PdfSurface::for_stream` wants an owned writer and hands it back at
/// `finish_output_stream`, so the buffer makes a round trip rather than being
/// shared.
#[derive(Default)]
struct Buffer(Vec<u8>);

impl io::Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Where the next thing goes, and what to do when it does not fit.
struct Cursor<'a> {
    context: &'a cairo::Context,
    y: f64,
    page: usize,
    title: Option<String>,
}

impl Cursor<'_> {
    fn bottom(&self) -> f64 {
        PAGE_HEIGHT - MARGIN
    }

    fn width(&self) -> f64 {
        PAGE_WIDTH - MARGIN * 2.0
    }

    /// Room for `height` more, or a new page first.
    fn ensure(&mut self, height: f64) -> Result<(), cairo::Error> {
        if self.y + height <= self.bottom() {
            return Ok(());
        }
        self.break_page()
    }

    fn break_page(&mut self) -> Result<(), cairo::Error> {
        self.footer()?;
        self.context.show_page()?;
        self.page += 1;
        self.y = MARGIN;
        Ok(())
    }

    /// The running foot: the document's title on the left, the page number on
    /// the right, both dimmed. It is drawn as a page is *finished*, so the
    /// number is always the number of the page it is on.
    fn footer(&mut self) -> Result<(), cairo::Error> {
        let layout = layout(self.context, SANS, FOOTER_SIZE);
        self.context.set_source_rgb(0.45, 0.45, 0.45);

        if let Some(title) = self.title.clone() {
            layout.set_text(&title);
            self.context.move_to(MARGIN, PAGE_HEIGHT - FOOTER);
            pangocairo::functions::show_layout(self.context, &layout);
        }

        let number = self.page.to_string();
        layout.set_text(&number);
        let (width, _) = layout.pixel_size();
        self.context
            .move_to(PAGE_WIDTH - MARGIN - f64::from(width), PAGE_HEIGHT - FOOTER);
        pangocairo::functions::show_layout(self.context, &layout);

        self.context.set_source_rgb(0.0, 0.0, 0.0);
        Ok(())
    }

    /// Paint a laid-out paragraph, breaking pages between its lines.
    ///
    /// `indent` shifts every line right; `first` is drawn instead of the first
    /// line's indent, which is how a bullet gets its marker without a second
    /// layout that could wrap differently.
    fn draw(
        &mut self,
        layout: &pango::Layout,
        indent: f64,
        marker: Option<&str>,
    ) -> Result<(), cairo::Error> {
        let scale = f64::from(pango::SCALE);
        let mut iter = layout.iter();
        let mut first = true;

        loop {
            let (_, logical) = iter.line_extents();
            let baseline = f64::from(iter.baseline()) / scale;
            let top = f64::from(logical.y()) / scale;
            let height = f64::from(logical.height()) / scale;

            self.ensure(height)?;

            if first {
                if let Some(marker) = marker {
                    let bullet = layout_of(self.context, SANS, BODY_SIZE, marker);
                    self.context.move_to(
                        MARGIN + indent - 14.0,
                        self.y + (baseline - top) - baseline_of(&bullet),
                    );
                    pangocairo::functions::show_layout(self.context, &bullet);
                }
                first = false;
            }

            // `show_layout_line` draws from the baseline, so the line's top is
            // wherever the cursor is and the baseline is that plus the ascent.
            self.context
                .move_to(MARGIN + indent, self.y + (baseline - top));
            if let Some(line) = iter.line_readonly() {
                pangocairo::functions::show_layout_line(self.context, &line);
            }
            self.y += height;

            if !iter.next_line() {
                break;
            }
        }
        Ok(())
    }
}

/// A finished PDF.
///
/// The page count comes back because it is the one fact about the result that
/// neither the model nor the user can see without opening the file, and "wrote
/// summary.pdf, 4 pages" is the difference between a tool that reports and one
/// that merely succeeds. Cairo writes page objects into compressed object
/// streams, so counting them afterwards would mean inflating the file.
pub struct Rendered {
    pub bytes: Vec<u8>,
    pub pages: usize,
}

/// Render blocks to a PDF.
///
/// `title` is drawn once at the top and then in the running foot of every page.
pub fn write(title: Option<&str>, blocks: &[Block]) -> Result<Rendered, Error> {
    let surface = cairo::PdfSurface::for_stream(PAGE_WIDTH, PAGE_HEIGHT, Buffer::default())
        .map_err(|error| Error::Surface(error.to_string()))?;
    let context = cairo::Context::new(&surface).map_err(|e| Error::Surface(e.to_string()))?;

    let title = title
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string);
    if let Some(title) = &title {
        surface.set_metadata(cairo::PdfMetadata::Title, title).ok();
    }
    surface
        .set_metadata(cairo::PdfMetadata::Creator, "Familiar")
        .ok();

    let mut cursor = Cursor {
        context: &context,
        y: MARGIN,
        page: 1,
        title: title.clone(),
    };

    paint(&mut cursor, title.as_deref(), blocks).map_err(|e| Error::Surface(e.to_string()))?;

    // The last page's foot is drawn here rather than in `break_page`, which
    // only ever runs when something else follows.
    cursor.footer().map_err(|e| Error::Surface(e.to_string()))?;
    let pages = cursor.page;
    drop(context);

    let stream = surface
        .finish_output_stream()
        .map_err(|error| Error::Surface(error.to_string()))?;
    let buffer = stream
        .downcast::<Buffer>()
        .map_err(|_| Error::Surface("the output stream came back as something else".into()))?;
    Ok(Rendered {
        bytes: buffer.0,
        pages,
    })
}

fn paint(cursor: &mut Cursor, title: Option<&str>, blocks: &[Block]) -> Result<(), cairo::Error> {
    if let Some(title) = title {
        let layout = layout(cursor.context, SANS, 24.0);
        layout.set_width((cursor.width() * f64::from(pango::SCALE)) as i32);
        let mut description = layout.font_description().unwrap_or_default();
        description.set_weight(pango::Weight::Bold);
        layout.set_font_description(Some(&description));
        layout.set_text(title);
        cursor.context.set_source_rgb(0.12, 0.22, 0.39);
        cursor.draw(&layout, 0.0, None)?;
        cursor.context.set_source_rgb(0.0, 0.0, 0.0);
        cursor.y += 18.0;
    }

    // A counter per nesting level, so `1.` `2.` `3.` come out as themselves.
    // The parser takes the number off the text and into the block type — Word
    // renumbers a list from its style and does not need it — so this is the
    // only place the numbering can be put back.
    let mut counters: Vec<usize> = Vec::new();
    for block in blocks {
        match block {
            Block::Numbered { level, .. } => {
                let level = (*level).min(4) as usize;
                counters.truncate(level + 1);
                while counters.len() <= level {
                    counters.push(0);
                }
                counters[level] += 1;
            }
            // A bullet inside a numbered list does not restart it; anything
            // else does, the way a paragraph between two lists does in
            // Markdown.
            Block::Bullet { .. } => {}
            _ => counters.clear(),
        }
        block_to_page(cursor, block, counters.last().copied().unwrap_or(1))?;
    }
    Ok(())
}

fn block_to_page(cursor: &mut Cursor, block: &Block, number: usize) -> Result<(), cairo::Error> {
    match block {
        Block::Heading { level, spans } => {
            let (size, above, bold) = heading_style(*level);
            cursor.y += above;
            let layout = markup_layout(cursor.context, SANS, size, spans, cursor.width());
            if bold {
                let mut description = layout.font_description().unwrap_or_default();
                description.set_weight(pango::Weight::Bold);
                layout.set_font_description(Some(&description));
            }
            // A heading at the very bottom of a page with its section on the
            // next one is the classic bad break. Keep it with what follows by
            // demanding room for a couple of lines, not just for itself.
            cursor.ensure(size * 3.0)?;
            cursor.context.set_source_rgb(0.12, 0.22, 0.39);
            cursor.draw(&layout, 0.0, None)?;
            cursor.context.set_source_rgb(0.0, 0.0, 0.0);
            cursor.y += 6.0;
        }
        Block::Paragraph { spans } => {
            let layout = markup_layout(cursor.context, SANS, BODY_SIZE, spans, cursor.width());
            cursor.draw(&layout, 0.0, None)?;
            cursor.y += 8.0;
        }
        Block::Quote { spans } => {
            let indent = 24.0;
            let layout = markup_layout(
                cursor.context,
                SANS,
                BODY_SIZE,
                spans,
                cursor.width() - indent,
            );
            let top = cursor.y;
            cursor.context.set_source_rgb(0.25, 0.25, 0.25);
            cursor.draw(&layout, indent, None)?;
            cursor.context.set_source_rgb(0.0, 0.0, 0.0);
            // The rule beside it is drawn after, so it spans exactly the text
            // that ended up on this page rather than a guess made before.
            if cursor.y > top {
                cursor.context.set_source_rgb(0.72, 0.72, 0.72);
                cursor.context.set_line_width(2.0);
                cursor.context.move_to(MARGIN + 8.0, top);
                cursor.context.line_to(MARGIN + 8.0, cursor.y);
                cursor.context.stroke()?;
                cursor.context.set_source_rgb(0.0, 0.0, 0.0);
            }
            cursor.y += 10.0;
        }
        Block::Bullet { level, spans } | Block::Numbered { level, spans } => {
            let numbered = matches!(block, Block::Numbered { .. });
            let indent = 18.0 + f64::from(*level) * 18.0;
            let layout = markup_layout(
                cursor.context,
                SANS,
                BODY_SIZE,
                spans,
                cursor.width() - indent,
            );
            // The number, counted by the caller, or the glyph for this depth.
            let counted;
            let marker = if numbered {
                counted = format!("{number}.");
                counted.as_str()
            } else {
                match level {
                    0 => "•",
                    1 => "◦",
                    _ => "▪",
                }
            };
            cursor.draw(&layout, indent, Some(marker))?;
            cursor.y += 4.0;
        }
        Block::Code { text, .. } => {
            let layout = layout(cursor.context, MONO, CODE_SIZE);
            layout.set_width(((cursor.width() - 20.0) * f64::from(pango::SCALE)) as i32);
            layout.set_wrap(pango::WrapMode::WordChar);
            layout.set_text(text);

            cursor.y += 4.0;
            let top = cursor.y;
            // Paint the shading first, per page, so a block that straddles a
            // break is shaded on both halves rather than neither.
            let (_, height) = layout.pixel_size();
            let fits = cursor.y + f64::from(height) <= cursor.bottom();
            let shaded = if fits {
                f64::from(height)
            } else {
                cursor.bottom() - cursor.y
            };
            if shaded > 0.0 {
                cursor.context.set_source_rgb(0.96, 0.96, 0.96);
                cursor
                    .context
                    .rectangle(MARGIN, top - 3.0, cursor.width(), shaded + 6.0);
                cursor.context.fill()?;
                cursor.context.set_source_rgb(0.0, 0.0, 0.0);
            }
            cursor.draw(&layout, 10.0, None)?;
            cursor.y += 10.0;
        }
        Block::Table { header, rows } => table(cursor, header, rows)?,
        Block::Rule => {
            cursor.ensure(14.0)?;
            cursor.y += 6.0;
            cursor.context.set_source_rgb(0.75, 0.75, 0.75);
            cursor.context.set_line_width(0.75);
            cursor.context.move_to(MARGIN, cursor.y);
            cursor.context.line_to(PAGE_WIDTH - MARGIN, cursor.y);
            cursor.context.stroke()?;
            cursor.context.set_source_rgb(0.0, 0.0, 0.0);
            cursor.y += 8.0;
        }
        Block::PageBreak => cursor.break_page()?,
    }
    Ok(())
}

/// A table, row by row, so a long one breaks across pages with its header
/// repeated — which is the only way a table over a page boundary stays legible.
fn table(cursor: &mut Cursor, header: &[String], rows: &[Vec<String>]) -> Result<(), cairo::Error> {
    let columns = header
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    if columns == 0 {
        return Ok(());
    }
    let width = cursor.width() / columns as f64;
    cursor.y += 6.0;

    if !header.is_empty() {
        table_row(cursor, header, columns, width, true)?;
    }
    for row in rows {
        // Not enough room for a row means a new page — and the header comes
        // with it, or the columns on page two mean nothing.
        if cursor.y + 30.0 > cursor.bottom() {
            cursor.break_page()?;
            if !header.is_empty() {
                table_row(cursor, header, columns, width, true)?;
            }
        }
        table_row(cursor, row, columns, width, false)?;
    }
    cursor.y += 10.0;
    Ok(())
}

fn table_row(
    cursor: &mut Cursor,
    cells: &[String],
    columns: usize,
    width: f64,
    header: bool,
) -> Result<(), cairo::Error> {
    let padding = 5.0;
    // Lay every cell out first: the row is as tall as its tallest cell, and
    // that is not knowable until all of them are measured.
    let layouts: Vec<pango::Layout> = (0..columns)
        .map(|column| {
            let text = cells.get(column).map(String::as_str).unwrap_or("");
            let spans = super::markup::spans(text);
            let spans: Vec<Span> = spans
                .into_iter()
                .map(|mut span| {
                    span.bold = span.bold || header;
                    span
                })
                .collect();
            markup_layout(cursor.context, SANS, 9.5, &spans, width - padding * 2.0)
        })
        .collect();

    let height = layouts
        .iter()
        .map(|layout| f64::from(layout.pixel_size().1))
        .fold(0.0, f64::max)
        + padding * 2.0;

    cursor.ensure(height)?;
    let top = cursor.y;

    if header {
        cursor.context.set_source_rgb(0.18, 0.33, 0.59);
        cursor
            .context
            .rectangle(MARGIN, top, cursor.width(), height);
        cursor.context.fill()?;
    }

    for (column, layout) in layouts.iter().enumerate() {
        let x = MARGIN + width * column as f64 + padding;
        if header {
            cursor.context.set_source_rgb(1.0, 1.0, 1.0);
        } else {
            cursor.context.set_source_rgb(0.0, 0.0, 0.0);
        }
        cursor.context.move_to(x, top + padding);
        pangocairo::functions::show_layout(cursor.context, layout);
    }

    // The grid, drawn after the text so a cell's border is never painted over.
    cursor.context.set_source_rgb(0.75, 0.75, 0.75);
    cursor.context.set_line_width(0.5);
    cursor
        .context
        .rectangle(MARGIN, top, cursor.width(), height);
    cursor.context.stroke()?;
    for column in 1..columns {
        let x = MARGIN + width * column as f64;
        cursor.context.move_to(x, top);
        cursor.context.line_to(x, top + height);
        cursor.context.stroke()?;
    }
    cursor.context.set_source_rgb(0.0, 0.0, 0.0);

    cursor.y = top + height;
    Ok(())
}

/// A layout at a family and size, with nothing in it yet.
fn layout(context: &cairo::Context, family: &str, size: f64) -> pango::Layout {
    let layout = pangocairo::functions::create_layout(context);
    let mut description = pango::FontDescription::from_string(family);
    // Absolute, in device units. The surface's unit is a point, so a size set
    // this way is a size in points — where `set_size` would go through the
    // context's 96 dpi resolution and come out a third too large.
    description.set_absolute_size(size * f64::from(pango::SCALE));
    layout.set_font_description(Some(&description));
    layout
}

fn layout_of(context: &cairo::Context, family: &str, size: f64, text: &str) -> pango::Layout {
    let layout = layout(context, family, size);
    layout.set_text(text);
    layout
}

/// The distance from a single-line layout's top to its baseline.
fn baseline_of(layout: &pango::Layout) -> f64 {
    f64::from(layout.baseline()) / f64::from(pango::SCALE)
}

/// A wrapped layout of spans, with the faces applied as Pango markup.
fn markup_layout(
    context: &cairo::Context,
    family: &str,
    size: f64,
    spans: &[Span],
    width: f64,
) -> pango::Layout {
    let layout = layout(context, family, size);
    layout.set_width((width.max(1.0) * f64::from(pango::SCALE)) as i32);
    layout.set_wrap(pango::WrapMode::WordChar);
    layout.set_markup(&markup(spans));
    layout
}

/// Spans as Pango markup.
///
/// Pango parses this, so an unescaped `<` from the model is a layout that
/// silently renders as nothing — this is the same hazard as the XML writers
/// and gets the same treatment.
fn markup(spans: &[Span]) -> String {
    let mut out = String::new();
    for span in spans {
        let text = super::xml::escape(&span.text);
        let mut open = String::new();
        let mut close = String::new();
        if span.bold {
            open.push_str("<b>");
            close.insert_str(0, "</b>");
        }
        if span.italic {
            open.push_str("<i>");
            close.insert_str(0, "</i>");
        }
        if span.code {
            open.push_str(&format!(
                "<span font_family=\"{MONO}\" background=\"#f2f2f2\">"
            ));
            close.insert_str(0, "</span>");
        }
        if span.link.is_some() {
            open.push_str("<span foreground=\"#0563c1\" underline=\"single\">");
            close.insert_str(0, "</span>");
        }
        out.push_str(&open);
        out.push_str(&text);
        out.push_str(&close);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::office::markup;

    fn build(markdown: &str) -> Rendered {
        write(Some("A Report"), &markup::parse(markdown)).expect("a PDF")
    }

    #[test]
    fn a_pdf_starts_with_the_header_and_ends_with_the_trailer() {
        let bytes = build("Hello.").bytes;
        assert_eq!(&bytes[..5], b"%PDF-", "not a PDF");
        let tail = String::from_utf8_lossy(&bytes[bytes.len() - 32..]);
        assert!(tail.contains("%%EOF"), "{tail}");
    }

    #[test]
    fn what_we_write_is_something_familiar_can_read_back() {
        // The same predicate the composer uses to decide a dropped file is a
        // document, so a PDF this app made can be dropped into this app.
        assert!(crate::model::documents::is_pdf(&build("Hello.").bytes));
    }

    #[test]
    fn a_short_document_is_one_page() {
        assert_eq!(build("# Title\n\nA sentence.").pages, 1);
    }

    #[test]
    fn a_long_document_breaks_across_pages_by_itself() {
        // The reason text is placed a line at a time: a paragraph longer than
        // the page must continue on the next one rather than overflow the
        // margin or push the whole paragraph down and leave a hole.
        let long = "Familiar renders an answer with Brain's scanner. ".repeat(400);
        assert!(build(&long).pages > 2);
    }

    #[test]
    fn an_explicit_page_break_starts_a_page() {
        assert_eq!(build("one\n\n\\pagebreak\n\ntwo").pages, 2);
    }

    #[test]
    fn every_block_kind_paints_without_erroring() {
        // The tables, rules, quotes and code paths all stroke and fill, and a
        // cairo error in any of them would surface here rather than as a file
        // that is half a document.
        let everything = "# Heading\n\nA paragraph with **bold**, *italic* and `code`.\n\n\
                          - a bullet\n  - nested\n\n1. numbered\n\n> a quote\n\n---\n\n\
                          ```rust\nlet x = 1;\n```\n\n| Name | Count |\n|---|---|\n| a | 1 |";
        let rendered = build(everything);
        assert_eq!(rendered.pages, 1);
        assert!(rendered.bytes.len() > 1_000, "suspiciously small");
    }

    #[test]
    fn a_table_longer_than_a_page_breaks_and_carries_its_header_over() {
        let mut markdown = String::from("| Name | Value |\n|---|---|\n");
        for row in 0..90 {
            markdown.push_str(&format!("| row {row} | {row} |\n"));
        }
        assert!(
            build(&markdown).pages > 1,
            "90 rows should not fit on one page"
        );
    }

    #[test]
    fn an_empty_document_is_still_a_valid_one_page_pdf() {
        let rendered = write(None, &[]).expect("a PDF");
        assert_eq!(&rendered.bytes[..5], b"%PDF-");
        assert_eq!(rendered.pages, 1);
    }

    /// The numbers a numbered list would be drawn with, in order.
    ///
    /// The counter is what the renderer feeds to the marker, so running the
    /// same walk is how a test sees it without reading the painted glyphs back
    /// out of a compressed content stream.
    fn numbering(markdown: &str) -> Vec<usize> {
        let mut counters: Vec<usize> = Vec::new();
        let mut seen = Vec::new();
        for block in markup::parse(markdown) {
            match block {
                Block::Numbered { level, .. } => {
                    let level = level.min(4) as usize;
                    counters.truncate(level + 1);
                    while counters.len() <= level {
                        counters.push(0);
                    }
                    counters[level] += 1;
                    seen.push(counters[level]);
                }
                Block::Bullet { .. } => {}
                _ => counters.clear(),
            }
        }
        seen
    }

    #[test]
    fn a_numbered_list_keeps_its_numbers() {
        // The parser moves the number out of the text and into the block type,
        // so drawing a bullet glyph here silently turned "1. 2. 3." into three
        // identical dots.
        assert_eq!(numbering("1. one\n2. two\n3. three"), [1, 2, 3]);
    }

    #[test]
    fn a_model_that_numbers_everything_one_still_gets_one_two_three() {
        // Markdown says a list numbered "1. 1. 1." renders 1, 2, 3, and a model
        // writes it that way often enough to matter.
        assert_eq!(numbering("1. one\n1. two\n1. three"), [1, 2, 3]);
    }

    #[test]
    fn a_paragraph_between_two_lists_restarts_the_numbering() {
        assert_eq!(numbering("1. one\n2. two\n\nAside.\n\n1. fresh"), [1, 2, 1]);
    }

    #[test]
    fn a_bullet_inside_a_numbered_list_does_not_restart_it() {
        assert_eq!(numbering("1. one\n  - a note\n2. two"), [1, 2]);
    }

    #[test]
    fn a_nested_numbered_list_counts_per_level() {
        assert_eq!(
            numbering("1. one\n  1. inner\n  2. inner\n2. two"),
            [1, 1, 2, 2]
        );
    }

    #[test]
    fn markup_that_pango_would_parse_is_escaped() {
        // An unescaped `<` makes Pango reject the markup and the line renders
        // as nothing at all — a silently blank paragraph.
        let escaped = markup(&[Span::plain("a < b & c")]);
        assert_eq!(escaped, "a &lt; b &amp; c");
        assert!(build("a < b & c").bytes.len() > 500);
    }

    #[test]
    fn faces_become_nested_markup_that_closes_in_order() {
        let out = markup(&[Span {
            text: "x".into(),
            bold: true,
            italic: true,
            ..Default::default()
        }]);
        assert_eq!(out, "<b><i>x</i></b>");
    }
}
