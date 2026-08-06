//! An answer, rendered.
//!
//! The model answers in Markdown and Brain already has a scanner that reports
//! which characters are *syntax* in character offsets — which is exactly what
//! `GtkTextBuffer::iter_at_offset` takes. So an answer is styled the way a note
//! is styled, and the two apps agree about what `**bold**` looks like because
//! they agree about what it *is*.
//!
//! What is not shared is this file. Brain's `highlight` reveals the markers
//! around the caret, caches per-line state for incremental re-scans, and styles
//! frontmatter — an editor's concerns. Here nothing is editable, so markers are
//! hidden unconditionally and a turn is scanned once when it settles.
//!
//! `GtkTextTag` takes colours rather than CSS classes, so these cannot come
//! from the stylesheet. They are derived from the view's own resolved
//! foreground and the accent colour, which keeps them right under any theme
//! including a high-contrast one — the same reasoning as Brain's.

use brain::model::markdown::{parse, LineState, Parsed, Style};
use gtk::gdk::RGBA;
use gtk::glib;
use gtk::prelude::*;

/// Applied to every character that is syntax rather than content. Unlike
/// Brain's, this one is never taken off.
const MARKER: &str = "md-marker";

/// Prefix of the tag that says where a link points. A `GtkTextTag` carries no
/// payload, so the destination is the tag's *name* and the click reads it back
/// off the character under the pointer. Tags move with the text they are on,
/// which is what keeps them right after [`lay_out_tables`] inserts an anchor
/// ahead of a table.
const TARGET: &str = "md-target:";

/// Colours that cannot live in the stylesheet.
struct Palette {
    dim: RGBA,
    accent: RGBA,
    code_background: RGBA,
}

impl Palette {
    /// Derived from the view's resolved foreground rather than from a
    /// light/dark branch containing black and white.
    fn of(widget: &impl IsA<gtk::Widget>) -> Self {
        let foreground = widget.as_ref().color();
        let tint = |alpha: f32| {
            RGBA::new(
                foreground.red(),
                foreground.green(),
                foreground.blue(),
                alpha,
            )
        };
        Self {
            dim: tint(0.65),
            accent: adw::StyleManager::default().accent_color_rgba(),
            code_background: tint(0.06),
        }
    }
}

/// Install the tags a rendered answer needs. Once per view.
pub fn install(view: &gtk::TextView) {
    let buffer = view.buffer();
    let table = buffer.tag_table();
    if table.lookup(MARKER).is_some() {
        return;
    }

    // A tag's name is construct-only, so each is built named rather than
    // renamed afterwards.
    let tag = |name: &str| {
        let tag = gtk::TextTag::new(Some(name));
        table.add(&tag);
        tag
    };

    // Syntax. Hidden, always: nobody is editing a reply.
    tag(MARKER).set_invisible(true);

    // A heading in an answer is a signpost between two paragraphs, not a title
    // page: weight carries it, and size only has to separate the three levels.
    for (name, scale) in [("md-h1", 1.35), ("md-h2", 1.2), ("md-h3", 1.08)] {
        let heading = tag(name);
        heading.set_scale(scale);
        heading.set_weight(700);
        // The blank line above a heading is shrunk with every other one; this
        // is the extra breath that keeps it with the text it introduces
        // rather than floating between two blocks.
        heading.set_pixels_above_lines(6);
        heading.set_pixels_below_lines(1);
    }

    // The blank line a paragraph break is made of, at a fraction of its height.
    // Left in rather than hidden: the gap between two paragraphs is what says
    // they are two, and a scale is a gap that follows the reader's font size.
    tag("md-blank").set_scale(0.45);

    tag("md-bold").set_weight(700);
    tag("md-italic").set_style(gtk::pango::Style::Italic);
    tag("md-strike").set_strikethrough(true);
    tag("md-code").set_family(Some("monospace"));

    let block = tag("md-codeblock");
    block.set_family(Some("monospace"));
    block.set_left_margin(12);

    // A text view cannot draw a border, so a quote is indented and italic
    // rather than ruled — Brain reached the same wall.
    let quote = tag("md-quote");
    quote.set_style(gtk::pango::Style::Italic);
    quote.set_left_margin(18);

    for name in ["md-link", "md-wikilink"] {
        tag(name).set_underline(gtk::pango::Underline::Single);
    }
    tag("md-tag");

    // One indent for every list depth: an answer's lists are shallow, and the
    // per-depth tags Brain needs are for notes people write by hand.
    let list = tag("md-list");
    list.set_left_margin(18);
    list.set_indent(-9);

    recolour(view);
}

/// Re-derive the colours. Cheap, and the scheme can change under a running app.
pub fn recolour(view: &gtk::TextView) {
    let palette = Palette::of(view);
    let table = view.buffer().tag_table();
    let set = |name: &str, apply: &dyn Fn(&gtk::TextTag)| {
        if let Some(tag) = table.lookup(name) {
            apply(&tag);
        }
    };

    set("md-code", &|tag| {
        tag.set_background_rgba(Some(&palette.code_background));
    });
    set("md-codeblock", &|tag| {
        tag.set_background_rgba(Some(&palette.code_background));
    });
    set("md-quote", &|tag| {
        tag.set_foreground_rgba(Some(&palette.dim));
    });
    for name in ["md-link", "md-wikilink", "md-tag"] {
        set(name, &|tag| {
            tag.set_foreground_rgba(Some(&palette.accent));
        });
    }
}

pub fn render(view: &gtk::TextView, text: &str) {
    let buffer = view.buffer();
    let parsed = parse(text);
    buffer.set_text(text);
    apply(&buffer, &parsed);
    shrink_gaps(&buffer, text, &parsed);
    aim(&buffer, text, &parsed);
    lay_out_tables(view, text, &parsed);
}

/// Take the space between blocks down to a gap, per [`gaps`].
fn shrink_gaps(buffer: &gtk::TextBuffer, text: &str, parsed: &Parsed) {
    let tags = buffer.tag_table();
    let (Some(blank), Some(marker)) = (tags.lookup("md-blank"), tags.lookup(MARKER)) else {
        return;
    };

    for (start, end, gap) in gaps(text, parsed) {
        let (Ok(start), Ok(end)) = (i32::try_from(start), i32::try_from(end)) else {
            continue;
        };
        let tag = match gap {
            Gap::Shrunk => &blank,
            Gap::Closed => &marker,
        };
        buffer.apply_tag(
            tag,
            &buffer.iter_at_offset(start),
            &buffer.iter_at_offset(end),
        );
    }
}

/// What becomes of a line that has nothing on it to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gap {
    /// A blank line: kept, at a fraction of a line's height.
    Shrunk,
    /// A line that is nothing but syntax: taken away, newline and all, so it
    /// closes up into the line below rather than leaving a band where the
    /// hidden characters used to be.
    Closed,
}

/// The character ranges an answer's vertical space can come out of.
///
/// The buffer holds the answer as the model wrote it, so a paragraph break is
/// a real empty line costing a real line of prose — over an answer with six
/// headings, a table and two code blocks in it, that is most of the scrolling.
/// The gap between two paragraphs is still what says they are two, so a blank
/// line is shrunk rather than removed; a fence, which says nothing once its
/// characters are hidden, goes altogether.
///
/// A blank line inside a fence is content and is left at full height.
fn gaps(text: &str, parsed: &Parsed) -> Vec<(usize, usize, Gap)> {
    let syntax = |at: usize| {
        parsed
            .markers
            .iter()
            .any(|marker| at >= marker.start && at < marker.end)
    };

    let mut found = Vec::new();
    let mut offset = 0usize;
    for (index, line) in text.split('\n').enumerate() {
        let start = offset;
        // Past the newline this line ends with: that character is what a blank
        // line is made of, and its font is what the line's height comes from.
        offset += line.chars().count() + 1;
        let fenced = parsed.line_states.get(index) == Some(&LineState::Fence);

        if line.trim().is_empty() {
            if !fenced {
                found.push((start, offset, Gap::Shrunk));
            }
            continue;
        }

        // Every character that would show is a marker, so nothing on this line
        // will be drawn.
        let shown = line
            .chars()
            .enumerate()
            .filter(|(_, character)| !character.is_whitespace())
            .any(|(at, _)| !syntax(start + at));
        if !shown {
            found.push((offset - 1, offset, Gap::Closed));
        }
    }
    found
}

/// Tag every link with where it goes.
fn aim(buffer: &gtk::TextBuffer, text: &str, parsed: &Parsed) {
    let table = buffer.tag_table();
    for (start, end, target) in targets(text, parsed) {
        let (Ok(start), Ok(end)) = (i32::try_from(start), i32::try_from(end)) else {
            continue;
        };
        let name = format!("{TARGET}{target}");
        let tag = table.lookup(&name).unwrap_or_else(|| {
            let tag = gtk::TextTag::new(Some(&name));
            table.add(&tag);
            tag
        });
        buffer.apply_tag(
            &tag,
            &buffer.iter_at_offset(start),
            &buffer.iter_at_offset(end),
        );
    }
}

/// Every link in `text` that leads anywhere: the characters a reader can click
/// on, and the URI they open.
///
/// `[label](target)` puts the target in the marker that follows the span, where
/// it is hidden — so the styled characters are the label and the destination is
/// read back out of the syntax around them. A bare URL has no marker and no
/// label: it is its own target.
fn targets(text: &str, parsed: &Parsed) -> Vec<(usize, usize, String)> {
    let characters: Vec<char> = text.chars().collect();
    let slice = |from: usize, to: usize| -> String {
        characters
            .get(from..to)
            .map_or_else(String::new, |slice| slice.iter().collect())
    };

    parsed
        .spans
        .iter()
        .filter(|span| span.style == Style::Link)
        .filter_map(|span| {
            let target = parsed
                .markers
                .iter()
                .find(|marker| marker.start == span.end)
                // `](` at the front and `)` at the back are not the URL.
                .map(|marker| slice(marker.start + 2, marker.end.saturating_sub(1)))
                .unwrap_or_else(|| slice(span.start, span.end));
            Some((span.start, span.end, launchable(&target)?))
        })
        .collect()
}

/// The URI a target opens, or `None` for one this app will not open.
///
/// An answer is not trusted input — the model repeats links out of search
/// results and out of mail, and a click is the user's, not the model's. So the
/// only schemes that go anywhere are the ones a link in prose is ever meant to
/// have. A target written with no scheme at all is the usual shape of a bare
/// domain and gets https; anything else with a colon in its head is a scheme
/// this app is declining to open.
fn launchable(target: &str) -> Option<String> {
    let target = target.trim();
    if target.is_empty() || target.contains(char::is_whitespace) {
        return None;
    }
    let lower = target.to_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:")
    {
        return Some(target.to_string());
    }
    // A relative path, an in-page anchor and a bare word are all links to
    // somewhere in a document that does not exist here. What is left has to
    // look like a host: labels with something in them, ending in a name rather
    // than a number or a port.
    let host = lower.split(['/', '?', '#']).next().unwrap_or_default();
    let labels: Vec<&str> = host.split('.').collect();
    let named = labels.len() > 1
        && labels.iter().all(|label| !label.is_empty())
        && labels
            .last()
            .is_some_and(|tld| tld.len() > 1 && tld.chars().all(|c| c.is_ascii_alphabetic()));
    named.then(|| format!("https://{target}"))
}

/// Where the point `x, y` in widget coordinates links to, if it links anywhere.
///
/// The obvious way to ask this is `iter_at_location`, and it cannot be used
/// here: on a buffer with invisible text in it — which is every answer, since
/// that is how the Markdown syntax is hidden — GTK's pixel-to-character
/// conversion computes a byte index past the end of the line and calls
/// `g_error`, which aborts the process. Pointing at a reply crashed the app.
///
/// So the question is asked the other way round. Every link is a tagged run
/// whose extent on the screen GTK will report, and a point is on a link when it
/// is inside one of those rectangles. Nothing converts a pixel to a character.
pub fn target_at(view: &gtk::TextView, x: f64, y: f64) -> Option<String> {
    let (x, y) = view.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
    links(view)
        .into_iter()
        .find(|(_, place)| place.iter().any(|rect| rect.contains_point(x, y)))
        .map(|(target, _)| target)
}

/// Every link in the view: where it goes, and where it is, in buffer
/// coordinates. Recomputed rather than cached, because where a link *is*
/// changes with every re-wrap.
///
/// Each link tag is followed by name rather than the buffer being walked for
/// toggles of any tag: an answer's every bold word and hidden marker is a
/// toggle, and this runs on every pointer motion.
fn links(view: &gtk::TextView) -> Vec<(String, Vec<gtk::gdk::Rectangle>)> {
    let buffer = view.buffer();
    let mut tags = Vec::new();
    buffer.tag_table().foreach(|tag| {
        if let Some(target) = tag
            .name()
            .and_then(|name| name.strip_prefix(TARGET).map(ToString::to_string))
        {
            tags.push((target, tag.clone()));
        }
    });

    let mut found = Vec::new();
    for (target, tag) in tags {
        let mut at = buffer.start_iter();
        // A tag left over from an earlier draft of a streaming answer is on
        // nothing at all, and this walk ends at the first question.
        if !at.starts_tag(Some(&tag)) && !at.forward_to_tag_toggle(Some(&tag)) {
            continue;
        }
        while at.starts_tag(Some(&tag)) {
            let mut end = at;
            if !end.forward_to_tag_toggle(Some(&tag)) {
                break;
            }
            found.push((target.clone(), extent(view, &at, &end)));
            at = end;
            if !at.forward_to_tag_toggle(Some(&tag)) {
                break;
            }
        }
    }
    found
}

/// The rectangles a run of text covers: one, or one per line it wraps onto.
///
/// A link that fits on a line — nearly all of them — costs two questions of the
/// layout, its first character and its last. Only one that wraps is walked
/// character by character, and then only to find where it breaks.
fn extent(
    view: &gtk::TextView,
    start: &gtk::TextIter,
    end: &gtk::TextIter,
) -> Vec<gtk::gdk::Rectangle> {
    let union = |first: gtk::gdk::Rectangle, last: gtk::gdk::Rectangle| {
        gtk::gdk::Rectangle::new(
            first.x(),
            first.y(),
            last.x() + last.width() - first.x(),
            first.height().max(last.height()),
        )
    };

    let mut before = *end;
    if !before.backward_char() {
        return Vec::new();
    }
    let (first, last) = (view.iter_location(start), view.iter_location(&before));
    if first.y() == last.y() {
        return vec![union(first, last)];
    }

    let mut rects: Vec<gtk::gdk::Rectangle> = Vec::new();
    let mut at = *start;
    while at.offset() < end.offset() {
        let here = view.iter_location(&at);
        // The space a wrapped link leaves at the end of a line is not part of
        // it, so a rectangle covers one line and the next one starts another.
        match rects.last_mut() {
            Some(last) if last.y() == here.y() => *last = union(*last, here),
            _ => rects.push(here),
        }
        if !at.forward_char() {
            break;
        }
    }
    rects
}

/// Make the links in an answer behave like links.
///
/// A `GtkTextView` draws tags and knows nothing about what they mean, so the
/// pointer, the tooltip and the click are all this. The click is on release and
/// only when nothing is selected: dragging across a link to copy it is not a
/// click on it.
fn wire_links(view: &gtk::TextView) {
    let click = gtk::GestureClick::new();
    click.set_button(gtk::gdk::BUTTON_PRIMARY);
    click.connect_released(|gesture, presses, x, y| {
        let Some(view) = gesture.widget().and_downcast::<gtk::TextView>() else {
            return;
        };
        if presses != 1 || view.buffer().has_selection() {
            return;
        }
        let Some(target) = target_at(&view, x, y) else {
            return;
        };
        gesture.set_state(gtk::EventSequenceState::Claimed);
        let window = view.root().and_downcast::<gtk::Window>();
        gtk::UriLauncher::new(&target).launch(
            window.as_ref(),
            gtk::gio::Cancellable::NONE,
            // A browser that will not open is the desktop's problem to report,
            // and it already has: there is nothing to put on the turn.
            |_| (),
        );
    });
    view.add_controller(click);

    let motion = gtk::EventControllerMotion::new();
    motion.connect_motion(|controller, x, y| {
        let Some(view) = controller.widget().and_downcast::<gtk::TextView>() else {
            return;
        };
        let over = target_at(&view, x, y);
        view.set_cursor_from_name(Some(if over.is_some() { "pointer" } else { "text" }));
    });
    view.add_controller(motion);

    // Where a link goes, before it is followed. The label is the model's words;
    // the destination is not, and this is the only place to read it.
    view.set_has_tooltip(true);
    view.connect_query_tooltip(|view, x, y, keyboard, tooltip| {
        if keyboard {
            return false;
        }
        let Some(target) = target_at(view, f64::from(x), f64::from(y)) else {
            return false;
        };
        tooltip.set_text(Some(&target));
        true
    });
}

/// Which way a column's cells sit, from the delimiter row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Align {
    Start,
    Centre,
    End,
}

/// A pipe table found in an answer: what it says, and where the source it was
/// read from lives so that source can be taken off the screen.
#[derive(Debug, PartialEq, Eq)]
struct Table {
    /// Char offsets, which is what `iter_at_offset` takes.
    start: usize,
    end: usize,
    /// The header first, then the body.
    rows: Vec<Vec<String>>,
    alignment: Vec<Align>,
}

/// Every table in `text`, in the order they appear.
///
/// The scanner reports `TableRow` and `TableDelimiter` spans, but a grid needs
/// the cells, not the extent of the line — so the block is read here. What the
/// scanner is asked for is which lines are *code*: a table quoted inside a
/// fence is someone being shown the source, and turning it into a grid destroys
/// the thing they were being shown.
fn tables(text: &str, parsed: &Parsed) -> Vec<Table> {
    let mut lines: Vec<(usize, &str)> = Vec::new();
    let mut offset = 0;
    for line in text.split('\n') {
        lines.push((offset, line));
        offset += line.chars().count() + 1;
    }

    let fenced = |at: usize| {
        parsed
            .spans
            .iter()
            .any(|span| span.style == Style::CodeBlock && at >= span.start && at < span.end)
    };
    let row = |index: usize| {
        lines
            .get(index)
            .is_some_and(|&(at, line)| is_row(line) && !fenced(at))
    };

    let mut found = Vec::new();
    let mut index = 0;
    while index + 1 < lines.len() {
        // A header alone is not a table. The delimiter row is what says the
        // pipes were meant as columns, and it is also what stops a half-typed
        // table turning into a grid one row at a time while the answer streams.
        if !row(index) || !row(index + 1) || !is_delimiter(lines[index + 1].1) {
            index += 1;
            continue;
        }

        let alignment = alignment_of(lines[index + 1].1);
        let mut rows = vec![cells_of(lines[index].1)];
        let mut last = index;
        let mut next = index + 2;
        while row(next) {
            rows.push(cells_of(lines[next].1));
            last = next;
            next += 1;
        }

        found.push(Table {
            start: lines[index].0,
            end: lines[last].0 + lines[last].1.chars().count(),
            rows,
            alignment,
        });
        index = next;
    }
    found
}

fn is_row(line: &str) -> bool {
    let line = line.trim();
    line.starts_with('|') && line.matches('|').count() >= 2
}

fn is_delimiter(line: &str) -> bool {
    let cells = cells_of(line);
    is_row(line)
        && !cells.is_empty()
        && cells.iter().all(|cell| {
            let dashes = cell.trim_start_matches(':').trim_end_matches(':');
            !dashes.is_empty() && dashes.chars().all(|character| character == '-')
        })
}

/// The cells on a row. `\|` is a pipe inside a cell, not a column break — get
/// that wrong and every column after it shifts along by one.
fn cells_of(line: &str) -> Vec<String> {
    let mut cells = vec![String::new()];
    let push = |cells: &mut Vec<String>, character| {
        cells
            .last_mut()
            .expect("a cell is always open")
            .push(character);
    };

    let mut escaped = false;
    for character in line.trim().chars() {
        match (escaped, character) {
            // Only the pipe is escapable here. A backslash before anything else
            // is a backslash, and answers are full of them.
            (true, '|') => push(&mut cells, '|'),
            (true, _) => {
                push(&mut cells, '\\');
                push(&mut cells, character);
            }
            (false, '\\') => {
                escaped = true;
                continue;
            }
            (false, '|') => cells.push(String::new()),
            (false, _) => push(&mut cells, character),
        }
        escaped = false;
    }
    if escaped {
        push(&mut cells, '\\');
    }

    // The outer pipes leave an empty cell at each end, which is not a column.
    if cells.first().is_some_and(|cell| cell.trim().is_empty()) {
        cells.remove(0);
    }
    if cells.len() > 1 && cells.last().is_some_and(|cell| cell.trim().is_empty()) {
        cells.pop();
    }
    cells.iter().map(|cell| cell.trim().to_string()).collect()
}

fn alignment_of(delimiter: &str) -> Vec<Align> {
    cells_of(delimiter)
        .iter()
        .map(|cell| match (cell.starts_with(':'), cell.ends_with(':')) {
            (true, true) => Align::Centre,
            (false, true) => Align::End,
            _ => Align::Start,
        })
        .collect()
}

fn lay_out_tables(view: &gtk::TextView, text: &str, parsed: &Parsed) {
    let buffer = view.buffer();
    let tags = buffer.tag_table();
    let (Some(marker), Some(blank)) = (tags.lookup(MARKER), tags.lookup("md-blank")) else {
        return;
    };

    // Back to front: the anchor is a character, and inserting one moves
    // everything after it.
    for table in tables(text, parsed).into_iter().rev() {
        let (Ok(start), Ok(end)) = (i32::try_from(table.start), i32::try_from(table.end)) else {
            continue;
        };
        // The anchor goes in first, so the source it displaces is hidden at the
        // offsets it has *after* the insertion.
        let anchor = buffer.create_child_anchor(&mut buffer.iter_at_offset(start));
        buffer.apply_tag(
            &marker,
            &buffer.iter_at_offset(start + 1),
            &buffer.iter_at_offset(end + 1),
        );
        // Hiding the source does not take its *lines* away: the newline that
        // ends the block is still visible, because it is what ends the line the
        // grid is on, and it still asks for a line of prose worth of height
        // under the table. Scaling that run down closes the band up without
        // touching the grid, which is a widget and takes no notice of a font.
        buffer.apply_tag(
            &blank,
            &buffer.iter_at_offset(start),
            &buffer.iter_at_offset(end + 2),
        );
        view.add_child_at_anchor(&grid(&table), &anchor);
    }
}

/// How many characters wide a cell may be before it wraps, given how many
/// columns are sharing the answer's measure. Roughly the width of the prose
/// above it, split up — wide enough for a sentence fragment in a two-column
/// table, narrow enough that eight columns still fit.
///
/// The floor matters as much as the budget. The view hands the grid its
/// minimum and then clips at its own width, so a table that asks for more than
/// the answer's measure loses its last columns off the side and says nothing
/// about it. Better a cramped column than a missing one.
fn cell_chars(columns: usize) -> i32 {
    let budget = 72 / columns.max(1);
    budget.clamp(8, 36) as i32
}

/// A cell as it should read, with the Markdown in it taken off the screen.
///
/// The rest of an answer hides its syntax with an invisible tag, which a label
/// has no equivalent of — so the markers are dropped from the string and the
/// styling they asked for comes back as Pango attributes. `**531**` is 531 in
/// bold, not four asterisks.
fn styled(cell: &str) -> (String, gtk::pango::AttrList) {
    let parsed = parse(cell);
    let dropped = |index: usize| {
        parsed
            .markers
            .iter()
            .any(|marker| index >= marker.start && index < marker.end)
    };

    // Where each source character ends up in the text that survives. A dropped
    // one maps to the position the next kept character will take, which is what
    // a span boundary wants.
    let mut text = String::new();
    let mut at = Vec::with_capacity(cell.chars().count() + 1);
    for (index, character) in cell.chars().enumerate() {
        at.push(text.len());
        if !dropped(index) {
            text.push(character);
        }
    }
    at.push(text.len());

    let styling = gtk::pango::AttrList::new();
    for span in &parsed.spans {
        let (Some(attribute), Some(&from), Some(&to)) =
            (attribute(span.style), at.get(span.start), at.get(span.end))
        else {
            continue;
        };
        let mut attribute = attribute;
        let (Ok(from), Ok(to)) = (u32::try_from(from), u32::try_from(to)) else {
            continue;
        };
        attribute.set_start_index(from);
        attribute.set_end_index(to);
        styling.insert(attribute);
    }
    (text, styling)
}

/// A cell that contains a link, written as Pango markup — or `None` for one
/// that does not.
///
/// The rest of a cell is plain text with attributes over it, and an attribute
/// list has no way to say "this run is a link": `GtkLabel` builds those from
/// `<a href>` in markup and from nothing else. So a cell with a link in it is
/// written out twice over — the same characters [`styled`] would keep, with the
/// styling as tags rather than attributes — and a cell without one keeps the
/// path it has always had.
///
/// The scanner's inline constructs never overlap: each is consumed whole before
/// the next begins. That is what makes writing an open tag at one offset and a
/// close tag at another produce markup that nests.
fn marked_up(cell: &str) -> Option<String> {
    let parsed = parse(cell);
    let links = targets(cell, &parsed);
    if links.is_empty() {
        return None;
    }

    let dropped = |index: usize| {
        parsed
            .markers
            .iter()
            .any(|marker| index >= marker.start && index < marker.end)
    };
    let tags = |start: usize, end: usize, style: Style| match style {
        Style::Bold => Some(("<b>".to_string(), "</b>")),
        Style::Italic => Some(("<i>".to_string(), "</i>")),
        Style::Strikethrough => Some(("<s>".to_string(), "</s>")),
        Style::Code => Some(("<tt>".to_string(), "</tt>")),
        Style::Link | Style::WikiLink => Some(
            links
                .iter()
                .find(|(from, to, _)| *from == start && *to == end)
                .map_or_else(
                    // Underlined and going nowhere, which is what it is: a note
                    // in a vault, or a scheme this app will not open.
                    || ("<u>".to_string(), "</u>"),
                    |(_, _, target)| {
                        let target = glib::markup_escape_text(target);
                        (format!("<a href=\"{target}\">"), "</a>")
                    },
                ),
        ),
        _ => None,
    };

    let mut markup = String::new();
    let mut closing: Vec<(usize, &str)> = Vec::new();
    for (index, character) in cell.chars().enumerate() {
        while closing.last().is_some_and(|&(at, _)| at == index) {
            markup.push_str(closing.pop().expect("just looked at it").1);
        }
        for span in &parsed.spans {
            if span.start == index {
                if let Some((open, close)) = tags(span.start, span.end, span.style) {
                    markup.push_str(&open);
                    closing.push((span.end, close));
                }
            }
        }
        if !dropped(index) {
            markup.push_str(&glib::markup_escape_text(&character.to_string()));
        }
    }
    while let Some((_, close)) = closing.pop() {
        markup.push_str(close);
    }
    Some(markup)
}

/// How a span reads inside a cell, or `None` for the ones a cell has no use
/// for — a heading or a list item in a table cell is not a thing.
fn attribute(style: Style) -> Option<gtk::pango::Attribute> {
    use gtk::pango::{AttrFontDesc, AttrInt, FontDescription};
    Some(match style {
        Style::Bold => AttrInt::new_weight(gtk::pango::Weight::Bold).upcast(),
        Style::Italic => AttrInt::new_style(gtk::pango::Style::Italic).upcast(),
        Style::Strikethrough => AttrInt::new_strikethrough(true).upcast(),
        Style::Code => AttrFontDesc::new(&FontDescription::from_string("monospace")).upcast(),
        Style::Link | Style::WikiLink => {
            AttrInt::new_underline(gtk::pango::Underline::Single).upcast()
        }
        _ => return None,
    })
}

fn grid(table: &Table) -> gtk::Widget {
    let grid = gtk::Grid::new();
    grid.add_css_class("md-table");
    grid.set_halign(gtk::Align::Start);

    let columns = table.rows.iter().map(Vec::len).max().unwrap_or(1);
    let cap = cell_chars(columns);

    for (row, cells) in table.rows.iter().enumerate() {
        // Every row gets every column, even the short ones. A row that stops
        // early would otherwise leave the rule above it stopping early too, and
        // a table with a hole punched in its side reads as a rendering fault
        // rather than as a row the model left ragged.
        for column in 0..columns {
            let source = cells.get(column).map_or("", String::as_str);
            let (cell, styling) = styled(source);
            let label = gtk::Label::new(Some(&cell));
            // Markup and attributes say the same thing about a cell; only
            // markup can also say that a run of it is a link. Either way the
            // characters on the screen are `cell`, which is what the width
            // below is measured from.
            match marked_up(source) {
                Some(markup) => {
                    label.set_markup(&markup);
                    // The gate on the answer's own links, on the path a cell's
                    // links take instead: `GtkLabel` opens an href itself, and
                    // this is where that can be stopped.
                    label.connect_activate_link(|_, uri| {
                        if launchable(uri).is_some() {
                            glib::Propagation::Proceed
                        } else {
                            glib::Propagation::Stop
                        }
                    });
                }
                None => label.set_attributes(Some(&styling)),
            }
            // A `GtkTextView` gives an anchored child its *minimum* width, and a
            // wrapping label's minimum is one character — which is how a table
            // came out as five columns of stacked letters. So a cell that fits
            // does not wrap at all, and its minimum is the text; only a cell of
            // prose wraps, to a width it also asks for as its minimum.
            let long = i32::try_from(cell.chars().count()).unwrap_or(i32::MAX) > cap;
            label.set_wrap(long);
            if long {
                label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
                label.set_width_chars(cap);
                label.set_max_width_chars(cap);
            }
            label.set_selectable(true);
            label.add_css_class("md-table-cell");

            let align = table.alignment.get(column).copied().unwrap_or(Align::Start);
            let (halign, xalign) = match align {
                Align::Start => (gtk::Align::Start, 0.0),
                Align::Centre => (gtk::Align::Center, 0.5),
                Align::End => (gtk::Align::End, 1.0),
            };
            label.set_halign(halign);
            label.set_xalign(xalign);

            if row == 0 {
                label.add_css_class("heading");
            } else {
                label.add_css_class("md-table-rule");
            }
            grid.attach(&label, column as i32, row as i32, 1, 1);
        }
    }
    grid.upcast()
}

/// Style what is already in the buffer.
fn apply(buffer: &gtk::TextBuffer, parsed: &Parsed) {
    let table = buffer.tag_table();

    let tag_at = |name: &str, start: usize, end: usize| {
        let (Ok(start), Ok(end)) = (i32::try_from(start), i32::try_from(end)) else {
            return;
        };
        if let Some(tag) = table.lookup(name) {
            buffer.apply_tag(
                &tag,
                &buffer.iter_at_offset(start),
                &buffer.iter_at_offset(end),
            );
        }
    };

    for span in &parsed.spans {
        if let Some(name) = tag_name(span.style) {
            tag_at(name, span.start, span.end);
        }
    }
    for marker in &parsed.markers {
        tag_at(MARKER, marker.start, marker.end);
    }
}

/// Which tag styles a span, or `None` for the ones an answer has no use for.
///
/// Frontmatter and embeds are a note's concerns: an answer that contains `---`
/// at the top is prose, not metadata. Table rows have no tag because a table is
/// not a run of styled characters — it is a grid, built in [`lay_out_tables`].
fn tag_name(style: Style) -> Option<&'static str> {
    Some(match style {
        Style::Heading(1) => "md-h1",
        Style::Heading(2) => "md-h2",
        Style::Heading(_) => "md-h3",
        Style::Bold => "md-bold",
        Style::Italic => "md-italic",
        Style::Strikethrough => "md-strike",
        Style::Code => "md-code",
        Style::CodeBlock => "md-codeblock",
        Style::Quote => "md-quote",
        Style::Link => "md-link",
        Style::WikiLink => "md-wikilink",
        Style::Tag => "md-tag",
        Style::ListItem(_) => "md-list",
        _ => return None,
    })
}

/// A `GtkTextView` set up to show an answer: styled, selectable, not editable.
pub fn view() -> gtk::TextView {
    let view = gtk::TextView::new();
    view.set_editable(false);
    view.set_cursor_visible(false);
    view.set_wrap_mode(gtk::WrapMode::WordChar);
    view.set_left_margin(0);
    view.set_right_margin(0);
    view.set_pixels_below_lines(2);
    view.add_css_class("turn-answer");
    install(&view);
    wire_links(&view);
    view
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain::model::markdown::parse;

    /// Every style the scanner can report is either styled here or knowingly
    /// dropped. A new `Style` in Brain should fail this, not silently render as
    /// plain prose.
    #[test]
    fn every_style_is_decided_about() {
        let known = [
            Style::Heading(1),
            Style::Heading(2),
            Style::Heading(6),
            Style::Bold,
            Style::Italic,
            Style::Strikethrough,
            Style::Code,
            Style::CodeBlock,
            Style::Quote,
            Style::Link,
            Style::WikiLink,
            Style::Tag,
            Style::ListItem(0),
        ];
        for style in known {
            assert!(tag_name(style).is_some(), "{style:?} has no tag");
        }

        let dropped = [
            Style::Frontmatter,
            Style::Embed,
            Style::TableRow,
            Style::TableDelimiter,
            Style::Rule,
            Style::Task(false),
        ];
        for style in dropped {
            assert!(tag_name(style).is_none(), "{style:?} is styled after all");
        }
    }

    fn linked(text: &str) -> Vec<(usize, usize, String)> {
        targets(text, &parse(text))
    }

    /// The label is what is on the screen and the target is hidden behind it,
    /// so a link that does not carry its destination separately is a link that
    /// goes nowhere.
    #[test]
    fn a_link_knows_where_it_points() {
        let found = linked("See [EuroNews — full article](https://euronews.com/ai) for more.");
        assert_eq!(found.len(), 1);
        let (start, end, target) = &found[0];
        assert_eq!(target, "https://euronews.com/ai");
        // The characters a reader can click on are the label, not the URL.
        let characters: Vec<char> =
            "See [EuroNews — full article](https://euronews.com/ai) for more."
                .chars()
                .collect();
        let label: String = characters[*start..*end].iter().collect();
        assert_eq!(label, "EuroNews — full article");
    }

    #[test]
    fn a_bare_url_is_its_own_target() {
        let found = linked("Read https://example.com/x, then decide.");
        assert_eq!(found.len(), 1);
        // The comma ends the sentence, not the URL.
        assert_eq!(found[0].2, "https://example.com/x");
    }

    /// An answer is a place where text the model read on the web comes back
    /// out, so what a click can be talked into opening is worth pinning down.
    #[test]
    fn only_a_link_to_the_web_goes_anywhere() {
        assert_eq!(
            launchable("euronews.com/ai"),
            Some("https://euronews.com/ai".to_string())
        );
        assert_eq!(
            launchable("mailto:someone@example.com"),
            Some("mailto:someone@example.com".to_string())
        );
        for refused in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "smb://host/share",
            "#section",
            "../notes/plan.md",
            "",
        ] {
            assert_eq!(launchable(refused), None, "{refused} is launchable");
        }
    }

    /// A cell's link cannot be an attribute over plain text — a label only
    /// makes a link out of `<a href>`.
    #[test]
    fn a_cell_with_a_link_in_it_is_written_as_markup() {
        let markup = marked_up("see [EuroNews](https://euronews.com/ai)").expect("a link");
        assert_eq!(
            markup,
            "see <a href=\"https://euronews.com/ai\">EuroNews</a>"
        );

        // Everything else the cell says still comes out with it.
        let markup = marked_up("**531** in [the report](report.example.com)").expect("a link");
        assert_eq!(
            markup,
            "<b>531</b> in <a href=\"https://report.example.com\">the report</a>"
        );

        // A link this app will not open is still not plain prose: it reads as a
        // link and goes nowhere, rather than silently losing its underline.
        let markup = marked_up("[the file](file:///etc/passwd) and [[Note]]");
        assert_eq!(markup, None, "nothing here is worth marking up");
    }

    /// Everything a cell can contain that markup would have to escape is in a
    /// cell somewhere: a comparison, an ampersand, a quoted flag.
    #[test]
    fn a_cell_marked_up_escapes_what_it_shows() {
        let markup = marked_up("a < b & \"c\" — [x](https://example.com)").expect("a link");
        assert!(
            markup.starts_with("a &lt; b &amp; &quot;c&quot; —"),
            "{markup}"
        );
        // What a reader ends up seeing is the cell, not the escapes.
        let (text, _) = styled("a < b & \"c\" — [x](https://example.com)");
        assert_eq!(text, "a < b & \"c\" — x");
    }

    /// A cell with nothing to link keeps the attribute path, which is the one
    /// every table in every answer so far has taken.
    #[test]
    fn a_cell_without_a_link_is_left_alone() {
        assert_eq!(marked_up("**531**"), None);
        assert_eq!(marked_up("Partly cloudy"), None);
    }

    /// A cell is not a run of characters in the buffer, so it cannot hide its
    /// syntax with a tag the way the prose does. It has to drop it instead.
    #[test]
    fn a_cell_reads_as_words_not_as_asterisks() {
        let (text, styling) = styled("**531**");
        assert_eq!(text, "531");
        assert!(styling.attributes().len() == 1, "the bold was lost");

        let (text, _) = styled("`basert` and *MLX*");
        assert_eq!(text, "basert and MLX");

        // Nothing to take off is the common case, and must survive it.
        let (text, styling) = styled("16,264");
        assert_eq!(text, "16,264");
        assert!(styling.attributes().is_empty());
    }

    const WEATHER: &str = "Today:\n\n\
        | Time | Temp | Conditions |\n\
        |------|-----:|:----------:|\n\
        | Morning | 67°F | Overcast |\n\
        | Midday | 76°F | Partly cloudy |\n\n\
        Bring a coat.";

    /// The scan takes the parse it is given, so the tests read the same way the
    /// renderer does.
    fn found(text: &str) -> Vec<Table> {
        tables(text, &parse(text))
    }

    #[test]
    fn a_table_is_read_as_cells_not_as_pipes() {
        let found = found(WEATHER);
        assert_eq!(found.len(), 1);
        let table = &found[0];
        assert_eq!(table.rows[0], ["Time", "Temp", "Conditions"]);
        assert_eq!(table.rows[2], ["Midday", "76°F", "Partly cloudy"]);
        assert_eq!(table.rows.len(), 3);
        assert_eq!(table.alignment, [Align::Start, Align::End, Align::Centre]);
    }

    #[test]
    fn a_table_knows_where_its_source_is() {
        let table = &found(WEATHER)[0];
        // Char offsets, so the degree signs count as one each.
        let chars: Vec<char> = WEATHER.chars().collect();
        let source: String = chars[table.start..table.end].iter().collect();
        assert!(source.starts_with("| Time |"), "{source:?}");
        assert!(source.ends_with("Partly cloudy |"), "{source:?}");
        // The prose either side is not part of it.
        assert!(!source.contains("Bring a coat"));
    }

    #[test]
    fn pipes_without_a_delimiter_row_are_prose() {
        // Half a table — which is what every table looks like for one frame
        // while the answer streams.
        assert!(found("| Time | Temp |\n| Morning | 67°F |").is_empty());
        assert!(found("a | b\n--- | ---").is_empty());
    }

    #[test]
    fn two_tables_are_two_tables() {
        let text = "| a |\n|---|\n| 1 |\n\nthen\n\n| b |\n|---|\n| 2 |";
        let found = found(text);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].rows[1], ["1"]);
        assert_eq!(found[1].rows[1], ["2"]);
    }

    /// Asking how a table is written gets you the pipes back, and a grid would
    /// be the one answer that cannot show them.
    #[test]
    fn a_table_inside_a_fence_stays_as_source() {
        let text = "Write it like this:\n\n```\n| a | b |\n|---|---|\n| 1 | 2 |\n```\n\nDone.";
        assert!(found(text).is_empty(), "{:?}", found(text));

        // The fence has to end for that to hold: a table after one is a table.
        let text = "```\n| a | b |\n|---|---|\n```\n\n| c | d |\n|---|---|\n| 3 | 4 |";
        let found = found(text);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].rows[0], ["c", "d"]);
    }

    #[test]
    fn an_escaped_pipe_is_a_pipe_not_a_column() {
        let table = &found("| a | b |\n|---|---|\n| x \\| y | 2 |")[0];
        assert_eq!(table.rows[1], ["x | y", "2"], "the columns shifted along");
    }

    /// The prose either side of a table is full of Windows paths and regexes.
    #[test]
    fn a_backslash_that_escapes_nothing_survives() {
        let table = &found("| a | b |\n|---|---|\n| C:\\Users | \\d+ |")[0];
        assert_eq!(table.rows[1], [r"C:\Users", r"\d+"]);
    }

    #[test]
    fn a_ragged_row_keeps_the_cells_it_has() {
        let table = &found("| a | b | c |\n|---|---|---|\n| 1 |\n| | | |")[0];
        assert_eq!(table.rows[1], ["1"]);
        // Three empty cells, not one and not none: the outer pipes are not
        // columns but the ones between them are.
        assert_eq!(table.rows[2], ["", "", ""]);
    }

    /// What an answer's vertical space is made of, decided over the source
    /// rather than in the buffer — so the case that matters most, a fence and
    /// the blank lines around it, can be checked without a display.
    #[test]
    fn a_blank_line_is_a_gap_and_a_fence_is_nothing() {
        let text = "One.\n\n```sh\nrun it\n\nagain\n```\n\nTwo.";
        let found = gaps(text, &parse(text));
        let of = |gap: Gap| -> Vec<(usize, usize)> {
            found
                .iter()
                .filter(|(_, _, kind)| *kind == gap)
                .map(|&(start, end, _)| (start, end))
                .collect()
        };

        // The two blank lines between blocks, and not the one inside the fence:
        // an empty line in a shell script is a line of the script.
        let characters: Vec<char> = text.chars().collect();
        assert_eq!(of(Gap::Shrunk), [(5, 6), (30, 31)]);
        for (start, end) in of(Gap::Shrunk) {
            let taken: String = characters[start..end].iter().collect();
            assert_eq!(taken, "\n", "{start}..{end} is not a blank line");
        }

        // Both fence lines close up, newline and all.
        assert_eq!(of(Gap::Closed).len(), 2);
        for (start, end) in of(Gap::Closed) {
            assert_eq!(characters[start..end], ['\n']);
        }
    }

    /// Prose is left alone. A line that reads as blank because everything on it
    /// is *styled* still has words on it.
    #[test]
    fn a_line_with_words_on_it_keeps_its_height() {
        let text = "**All bold.**\n# A heading\n- an item";
        assert_eq!(gaps(text, &parse(text)), []);
    }

    #[test]
    fn an_answer_scans_the_way_a_note_does() {
        // Not a test of this module so much as of the assumption it rests on:
        // the scanner reports markers for the syntax, in char offsets, so
        // hiding them leaves the prose.
        let parsed = parse("A **bold** claim.");
        assert!(parsed
            .spans
            .iter()
            .any(|span| span.style == Style::Bold && span.start == 4 && span.end == 8));
        assert_eq!(parsed.markers.len(), 2);
    }
}
