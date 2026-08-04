//! Making text safe to put in XML.
//!
//! Every part of every Office file is XML, and all of the text in one comes
//! from a model. `escape` is the only thing standing between an answer that
//! mentions `<w:p>` and a document Word refuses to open — so it is one function
//! that every writer calls, rather than a habit each of them has to remember.

/// The five predefined entities, plus the control characters XML 1.0 forbids.
///
/// Ampersand first: escaping it after the others would double-escape what they
/// just produced.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // Tab, newline and carriage return are the only control characters
            // XML 1.0 allows. The rest are not representable at all — not even
            // as a numeric reference — so they are dropped rather than encoded
            // into a file that will not parse.
            '\t' | '\n' | '\r' => out.push(character),
            c if (c as u32) < 0x20 => {}
            // Unpaired surrogates cannot occur in a Rust `str`, so what is left
            // is valid.
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_five_predefined_entities_are_escaped() {
        assert_eq!(
            escape(r#"<a href="x">&'</a>"#),
            "&lt;a href=&quot;x&quot;&gt;&amp;&apos;&lt;/a&gt;"
        );
    }

    #[test]
    fn an_ampersand_is_not_escaped_twice() {
        // Escaping `<` first would turn it into `&lt;` and then into
        // `&amp;lt;`, which Word renders as the literal text "&lt;".
        assert_eq!(escape("a < b"), "a &lt; b");
        assert_eq!(escape("&amp;"), "&amp;amp;");
    }

    #[test]
    fn whitespace_that_xml_allows_survives() {
        assert_eq!(escape("a\tb\nc\r"), "a\tb\nc\r");
    }

    #[test]
    fn a_control_character_is_dropped_rather_than_written() {
        // XML 1.0 cannot represent these at all, so a document containing one
        // does not parse — for any reader, not just Word.
        assert_eq!(escape("a\u{0}b\u{7}c\u{1b}d"), "abcd");
    }

    #[test]
    fn ordinary_text_is_untouched() {
        assert_eq!(
            escape("Straightforward — with an em dash."),
            "Straightforward — with an em dash."
        );
    }
}
