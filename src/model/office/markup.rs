//! The shape of a document, and the Markdown a model writes it in.
//!
//! A model does not think in `w:p` elements, and asking it to emit OOXML would
//! be asking a 30B to be a serialiser. It thinks in Markdown — it has written
//! more of it than anything else — so a tool takes Markdown and this module
//! turns it into [`Block`]s.
//!
//! Every writer consumes the same blocks: `.docx`, `.pptx` and the PDF layout
//! all read this one spec, which is why the same content produces a Word file
//! and a PDF that agree. An external converter could not promise that.
//!
//! The parser is deliberately small. It handles what an assistant actually
//! writes — headings, paragraphs, lists, tables, code, quotes, rules — and it
//! never fails: anything it does not recognise is a paragraph of text, because
//! a document with an odd line in it is better than an error.

/// A piece of text with a face on it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Span {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    /// Inline `code`, rendered in a mono face.
    pub code: bool,
    /// Where a link pointed, if this span was one.
    pub link: Option<String>,
}

impl Span {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Default::default()
        }
    }

    pub fn bold(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            bold: true,
            ..Default::default()
        }
    }
}

/// One block of a document, in the order it appears.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// `level` is 1..=6, already clamped.
    Heading {
        level: u8,
        spans: Vec<Span>,
    },
    Paragraph {
        spans: Vec<Span>,
    },
    /// `level` is the nesting depth, from 0.
    Bullet {
        level: u8,
        spans: Vec<Span>,
    },
    Numbered {
        level: u8,
        spans: Vec<Span>,
    },
    Quote {
        spans: Vec<Span>,
    },
    /// A fenced block, kept verbatim. `language` is the fence's info string.
    Code {
        language: Option<String>,
        text: String,
    },
    /// A pipe table. `header` is empty when the table had no header row.
    Table {
        header: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    /// A horizontal rule, `---` on its own line.
    Rule,
    /// An explicit page break, written as `\pagebreak` on its own line.
    PageBreak,
}

impl Block {
    /// The block's text with every face dropped — what a plain reader sees.
    pub fn text(&self) -> String {
        match self {
            Self::Heading { spans, .. }
            | Self::Paragraph { spans }
            | Self::Bullet { spans, .. }
            | Self::Numbered { spans, .. }
            | Self::Quote { spans } => spans.iter().map(|span| span.text.as_str()).collect(),
            Self::Code { text, .. } => text.clone(),
            Self::Table { header, rows } => {
                let mut all: Vec<String> = Vec::new();
                if !header.is_empty() {
                    all.push(header.join(" | "));
                }
                all.extend(rows.iter().map(|row| row.join(" | ")));
                all.join("\n")
            }
            Self::Rule | Self::PageBreak => String::new(),
        }
    }
}

/// Turn Markdown into blocks.
///
/// Never fails. A line that is not any recognised construct is a paragraph,
/// which is the only sane reading of "the model wrote something unexpected".
pub fn parse(markdown: &str) -> Vec<Block> {
    let lines: Vec<&str> = markdown.lines().collect();
    let mut blocks = Vec::new();
    let mut paragraph: Vec<String> = Vec::new();
    let mut index = 0;

    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim();

        // A fence swallows lines verbatim until it closes, so nothing inside a
        // code block is ever read as Markdown.
        if let Some(info) = fence(trimmed) {
            flush(&mut paragraph, &mut blocks);
            let mut body = Vec::new();
            index += 1;
            while index < lines.len() && fence(lines[index].trim()).is_none() {
                body.push(lines[index]);
                index += 1;
            }
            // A fence that never closes still produces its block: the document
            // is what the model meant, not what it punctuated.
            index += 1;
            blocks.push(Block::Code {
                language: (!info.is_empty()).then(|| info.to_string()),
                text: body.join("\n"),
            });
            continue;
        }

        if trimmed.is_empty() {
            flush(&mut paragraph, &mut blocks);
            index += 1;
            continue;
        }

        if trimmed == r"\pagebreak" || trimmed == "\\newpage" {
            flush(&mut paragraph, &mut blocks);
            blocks.push(Block::PageBreak);
            index += 1;
            continue;
        }

        if is_rule(trimmed) {
            flush(&mut paragraph, &mut blocks);
            blocks.push(Block::Rule);
            index += 1;
            continue;
        }

        if let Some((level, rest)) = heading(trimmed) {
            flush(&mut paragraph, &mut blocks);
            blocks.push(Block::Heading {
                level,
                spans: spans(rest),
            });
            index += 1;
            continue;
        }

        // A table is the only construct that needs to look ahead: `| a | b |`
        // is a paragraph unless a delimiter row follows it.
        if is_row(trimmed)
            && lines
                .get(index + 1)
                .is_some_and(|next| is_delimiter(next.trim()))
        {
            flush(&mut paragraph, &mut blocks);
            let header = cells(trimmed);
            index += 2;
            let mut rows = Vec::new();
            while index < lines.len() && is_row(lines[index].trim()) {
                rows.push(cells(lines[index].trim()));
                index += 1;
            }
            blocks.push(Block::Table { header, rows });
            continue;
        }

        if let Some((level, rest, ordered)) = item(line) {
            flush(&mut paragraph, &mut blocks);
            blocks.push(if ordered {
                Block::Numbered {
                    level,
                    spans: spans(rest),
                }
            } else {
                Block::Bullet {
                    level,
                    spans: spans(rest),
                }
            });
            index += 1;
            continue;
        }

        if let Some(rest) = trimmed
            .strip_prefix("> ")
            .or_else(|| (trimmed == ">").then_some(""))
        {
            flush(&mut paragraph, &mut blocks);
            blocks.push(Block::Quote { spans: spans(rest) });
            index += 1;
            continue;
        }

        paragraph.push(trimmed.to_string());
        index += 1;
    }

    flush(&mut paragraph, &mut blocks);
    blocks
}

/// Split one line of text into spans by its inline markup.
///
/// `**bold**`, `*italic*`, `_italic_`, `` `code` `` and `[text](url)`. A marker
/// that never closes is text, not an error — half a document should not lose
/// its formatting because a model wrote one stray asterisk.
pub fn spans(text: &str) -> Vec<Span> {
    let characters: Vec<char> = text.chars().collect();
    let mut spans: Vec<Span> = Vec::new();
    let mut plain = String::new();
    let mut index = 0;

    let push = |plain: &mut String, span: Option<Span>, spans: &mut Vec<Span>| {
        if !plain.is_empty() {
            spans.push(Span::plain(std::mem::take(plain)));
        }
        if let Some(span) = span {
            spans.push(span);
        }
    };

    while index < characters.len() {
        // A backslash escapes the next character, which is how a document says
        // "an asterisk, not emphasis".
        if characters[index] == '\\' && index + 1 < characters.len() {
            plain.push(characters[index + 1]);
            index += 2;
            continue;
        }

        // Code first: nothing inside backticks is markup.
        if characters[index] == '`' {
            if let Some(close) = find(&characters, index + 1, &['`']) {
                push(
                    &mut plain,
                    Some(Span {
                        text: characters[index + 1..close].iter().collect(),
                        code: true,
                        ..Default::default()
                    }),
                    &mut spans,
                );
                index = close + 1;
                continue;
            }
        }

        if characters[index] == '*' && characters.get(index + 1) == Some(&'*') {
            if let Some(close) = find_pair(&characters, index + 2, '*') {
                push(
                    &mut plain,
                    Some(Span {
                        text: characters[index + 2..close].iter().collect(),
                        bold: true,
                        ..Default::default()
                    }),
                    &mut spans,
                );
                index = close + 2;
                continue;
            }
        }

        if characters[index] == '*' || characters[index] == '_' {
            let marker = characters[index];
            if let Some(close) = find(&characters, index + 1, &[marker]) {
                // `snake_case_words` are not emphasis, and a model writes them
                // constantly. An underscore between word characters is text.
                let inside_word =
                    marker == '_' && index > 0 && characters[index - 1].is_alphanumeric();
                if !inside_word {
                    push(
                        &mut plain,
                        Some(Span {
                            text: characters[index + 1..close].iter().collect(),
                            italic: true,
                            ..Default::default()
                        }),
                        &mut spans,
                    );
                    index = close + 1;
                    continue;
                }
            }
        }

        if characters[index] == '[' {
            if let Some(close) = find(&characters, index + 1, &[']']) {
                if characters.get(close + 1) == Some(&'(') {
                    if let Some(end) = find(&characters, close + 2, &[')']) {
                        push(
                            &mut plain,
                            Some(Span {
                                text: characters[index + 1..close].iter().collect(),
                                link: Some(characters[close + 2..end].iter().collect()),
                                ..Default::default()
                            }),
                            &mut spans,
                        );
                        index = end + 1;
                        continue;
                    }
                }
            }
        }

        plain.push(characters[index]);
        index += 1;
    }

    push(&mut plain, None, &mut spans);
    if spans.is_empty() {
        spans.push(Span::plain(""));
    }
    spans
}

fn find(characters: &[char], from: usize, any_of: &[char]) -> Option<usize> {
    (from..characters.len()).find(|index| any_of.contains(&characters[*index]))
}

/// The close of a `**` run.
fn find_pair(characters: &[char], from: usize, marker: char) -> Option<usize> {
    (from..characters.len().saturating_sub(1))
        .find(|index| characters[*index] == marker && characters[index + 1] == marker)
}

fn flush(paragraph: &mut Vec<String>, blocks: &mut Vec<Block>) {
    if paragraph.is_empty() {
        return;
    }
    // Wrapped lines are one paragraph, the way Markdown means them.
    let joined = std::mem::take(paragraph).join(" ");
    blocks.push(Block::Paragraph {
        spans: spans(&joined),
    });
}

fn fence(trimmed: &str) -> Option<&str> {
    trimmed
        .strip_prefix("```")
        .or_else(|| trimmed.strip_prefix("~~~"))
        .map(str::trim)
}

fn heading(trimmed: &str) -> Option<(u8, &str)> {
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = trimmed[hashes..].strip_prefix(' ')?;
    Some((hashes as u8, rest.trim()))
}

fn is_rule(trimmed: &str) -> bool {
    let stripped: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    stripped.len() >= 3
        && (stripped.chars().all(|c| c == '-')
            || stripped.chars().all(|c| c == '*')
            || stripped.chars().all(|c| c == '_'))
}

/// A list item, with its nesting taken from the indent.
///
/// Four spaces or one tab is a level, which is what every editor produces and
/// what a model imitates.
fn item(line: &str) -> Option<(u8, &str, bool)> {
    let indent: usize = line
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum();
    let trimmed = line.trim_start();
    let level = (indent / 2).min(4) as u8;

    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(marker) {
            return Some((level, rest.trim(), false));
        }
    }

    let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 {
        let rest = &trimmed[digits..];
        for marker in [". ", ") "] {
            if let Some(rest) = rest.strip_prefix(marker) {
                return Some((level, rest.trim(), true));
            }
        }
    }
    None
}

fn is_row(trimmed: &str) -> bool {
    trimmed.starts_with('|') && trimmed.len() > 1
}

/// `|---|:--:|` and friends: the row that makes the one above it a header.
fn is_delimiter(trimmed: &str) -> bool {
    is_row(trimmed)
        && cells(trimmed).iter().all(|cell| {
            !cell.is_empty()
                && cell
                    .chars()
                    .all(|c| c == '-' || c == ':' || c.is_whitespace())
                && cell.contains('-')
        })
}

fn cells(trimmed: &str) -> Vec<String> {
    let inner = trimmed.trim().trim_start_matches('|').trim_end_matches('|');
    inner
        .split('|')
        .map(|cell| cell.trim().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(blocks: &[Block]) -> Vec<String> {
        blocks.iter().map(Block::text).collect()
    }

    #[test]
    fn headings_carry_their_level() {
        let blocks = parse("# Title\n\n### Deep");
        assert_eq!(
            blocks,
            vec![
                Block::Heading {
                    level: 1,
                    spans: vec![Span::plain("Title")]
                },
                Block::Heading {
                    level: 3,
                    spans: vec![Span::plain("Deep")]
                },
            ]
        );
    }

    #[test]
    fn seven_hashes_are_not_a_heading() {
        // There is no h7, and a row of hashes is more likely to be text.
        let blocks = parse("####### not a heading");
        assert!(matches!(blocks[0], Block::Paragraph { .. }));
    }

    #[test]
    fn a_hash_with_no_space_is_a_paragraph() {
        assert!(matches!(parse("#hashtag")[0], Block::Paragraph { .. }));
    }

    #[test]
    fn wrapped_lines_become_one_paragraph() {
        let blocks = parse("The scanner reports\nsyntax spans in char\noffsets.");
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            texts(&blocks)[0],
            "The scanner reports syntax spans in char offsets."
        );
    }

    #[test]
    fn a_blank_line_ends_a_paragraph() {
        let blocks = parse("First.\n\nSecond.");
        assert_eq!(texts(&blocks), ["First.", "Second."]);
    }

    #[test]
    fn bullets_and_numbers_are_told_apart_and_keep_their_depth() {
        let blocks = parse("- top\n  - nested\n1. first\n2) second");
        assert_eq!(
            blocks,
            vec![
                Block::Bullet {
                    level: 0,
                    spans: vec![Span::plain("top")]
                },
                Block::Bullet {
                    level: 1,
                    spans: vec![Span::plain("nested")]
                },
                Block::Numbered {
                    level: 0,
                    spans: vec![Span::plain("first")]
                },
                Block::Numbered {
                    level: 0,
                    spans: vec![Span::plain("second")]
                },
            ]
        );
    }

    #[test]
    fn a_fence_keeps_its_contents_verbatim() {
        // The whole point: nothing inside is read as Markdown.
        let blocks = parse("```rust\nlet x = *p;\n# not a heading\n```\nafter");
        assert_eq!(
            blocks[0],
            Block::Code {
                language: Some("rust".into()),
                text: "let x = *p;\n# not a heading".into(),
            }
        );
        assert_eq!(texts(&blocks)[1], "after");
    }

    #[test]
    fn a_fence_that_never_closes_still_produces_its_block() {
        let blocks = parse("```\nunterminated");
        assert_eq!(
            blocks[0],
            Block::Code {
                language: None,
                text: "unterminated".into()
            }
        );
    }

    #[test]
    fn a_table_needs_its_delimiter_row() {
        let table = parse("| a | b |\n|---|---|\n| 1 | 2 |");
        assert_eq!(
            table[0],
            Block::Table {
                header: vec!["a".into(), "b".into()],
                rows: vec![vec!["1".into(), "2".into()]],
            }
        );

        // Without one it is text that happens to contain pipes.
        let not_a_table = parse("| a | b |\n| 1 | 2 |");
        assert!(matches!(not_a_table[0], Block::Paragraph { .. }));
    }

    #[test]
    fn an_aligned_delimiter_row_still_makes_a_table() {
        let blocks = parse("| left | right |\n|:--- | ---:|\n| 1 | 2 |");
        assert!(matches!(blocks[0], Block::Table { .. }));
    }

    #[test]
    fn rules_and_page_breaks_are_their_own_blocks() {
        let blocks = parse("one\n\n---\n\n\\pagebreak\n\ntwo");
        assert_eq!(blocks[1], Block::Rule);
        assert_eq!(blocks[2], Block::PageBreak);
    }

    #[test]
    fn emphasis_becomes_spans() {
        let spans = spans("plain **bold** and *italic* and `code`");
        assert_eq!(spans[0], Span::plain("plain "));
        assert_eq!(spans[1], Span::bold("bold"));
        assert!(spans[3].italic);
        assert_eq!(spans[3].text, "italic");
        assert!(spans[5].code);
        assert_eq!(spans[5].text, "code");
    }

    #[test]
    fn an_underscore_inside_a_word_is_not_emphasis() {
        // A model writes `messages_for_model` constantly, and turning half of
        // it italic mangles the sentence.
        let spans = spans("call messages_for_model here");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "call messages_for_model here");
    }

    #[test]
    fn an_unclosed_marker_is_text_not_a_lost_half_document() {
        let spans = spans("2 * 3 = 6");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "2 * 3 = 6");
        assert!(!spans[0].italic);
    }

    #[test]
    fn nothing_inside_backticks_is_markup() {
        let spans = spans("`**not bold**`");
        assert_eq!(spans.len(), 1);
        assert!(spans[0].code);
        assert_eq!(spans[0].text, "**not bold**");
    }

    #[test]
    fn a_link_keeps_its_text_and_its_target() {
        let spans = spans("see [the design](https://example.com/d) for more");
        assert_eq!(spans[1].text, "the design");
        assert_eq!(spans[1].link.as_deref(), Some("https://example.com/d"));
        assert_eq!(spans[2].text, " for more");
    }

    #[test]
    fn a_backslash_escapes_a_marker() {
        let spans = spans(r"a \* b");
        assert_eq!(spans[0].text, "a * b");
    }

    #[test]
    fn quotes_are_kept_as_quotes() {
        let blocks = parse("> Nothing async, and no HTTP stack.");
        assert_eq!(
            blocks[0],
            Block::Quote {
                spans: vec![Span::plain("Nothing async, and no HTTP stack.")]
            }
        );
    }

    #[test]
    fn empty_markdown_is_no_blocks_rather_than_an_error() {
        assert!(parse("").is_empty());
        assert!(parse("   \n\n  ").is_empty());
    }
}
