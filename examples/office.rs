//! Write one of each document, so they can be opened and looked at.
//!
//! The unit tests assert on the XML inside the archive, which proves what was
//! written and not that Word will open it. This is the other half: it produces
//! the real files, and the only way to finish the check is to open them.
//!
//! ```sh
//! cargo run --example office -- /tmp/out
//! # then, if LibreOffice is installed, a machine-checkable version of "does it
//! # open?" — it fails on a malformed package rather than rendering it anyway:
//! flatpak run org.libreoffice.LibreOffice --headless --convert-to pdf \
//!     --outdir /tmp/out/check /tmp/out/*.docx /tmp/out/*.xlsx /tmp/out/*.pptx
//! ```
//!
//! `examples/preview.rs` is the same idea for the widgets, and for the same
//! reason: "does this look right?" is not a question a test can answer.

use familiar::model::office::{docx, markup, pdf, pptx, xlsx};

const REPORT: &str = r#"
## Summary

Familiar renders an answer with Brain's Markdown scanner, which reports which
characters are *syntax* in char offsets — exactly what a `GtkTextBuffer` wants.

Three things carried over from `llamatui`, and two did not:

- Compaction's shape: a rolling summary, folded at turn boundaries
  - The recent window is never touched
  - A context overflow escalates to a floor and retries once
- The **wire adapter** as one module
- Hybrid semantic recall, which was dropped

1. Read the vault
2. Build the index in memory
3. Answer

> Nothing async, and no HTTP stack.

| Subsystem | Lines | Needs a display |
|---|---|---|
| `model/turn.rs` | 861 | no |
| `model/office/` | 2,400 | no |
| `ui/application.rs` | 1,636 | yes |

---

```rust
let plan = documents::plan(info.pages, &pages);
assert!(plan.to_rasterise.len() <= MAX_IMAGE_PAGES);
```

\pagebreak

## Second page

This paragraph is on page two because the line above asked for a break.
See [the design](https://example.com/design) for the rest.
"#;

fn main() {
    let directory = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/familiar-office".to_string());
    let directory = std::path::Path::new(&directory);
    std::fs::create_dir_all(directory).expect("a directory to write into");

    let blocks = markup::parse(REPORT);
    println!("{} blocks parsed from the Markdown\n", blocks.len());

    write(
        directory,
        "report.docx",
        docx::write(Some("Q3 Review"), &blocks),
    );

    let rendered = pdf::write(Some("Q3 Review"), &blocks).expect("a PDF");
    println!("  (the PDF came out at {} pages)", rendered.pages);
    write(directory, "report.pdf", rendered.bytes);

    let mut sheet = xlsx::Sheet::new("Pipeline");
    for row in [
        vec!["Region", "Deals", "Value", "Won"],
        vec!["North", "12", "£48,250.50", "TRUE"],
        vec!["South", "7", "31900", "false"],
        vec!["East", "0", "0", "false"],
        // The awkward ones: a leading zero must stay text, and a formula must
        // stay a formula.
        vec!["Ref 007", "3", "12%", "TRUE"],
        vec!["Total", "=SUM(B2:B5)", "=SUM(C2:C5)", ""],
    ] {
        sheet
            .rows
            .push(row.iter().map(|text| xlsx::Cell::infer(text)).collect());
    }
    write(directory, "pipeline.xlsx", xlsx::write(&[sheet]));

    let deck = vec![
        pptx::Slide {
            title: "Familiar".into(),
            bullets: Vec::new(),
            notes: Some("A title slide has no bullets, so it gets no content box.".into()),
        },
        pptx::Slide {
            title: "Revenue rose 12% on new accounts".into(),
            bullets: vec![
                pptx::Bullet::new(0, "Three of five milestones done"),
                pptx::Bullet::new(1, "Compaction landed last week"),
                pptx::Bullet::new(1, "**Reasoning** is carried, and that halved re-derivation"),
                pptx::Bullet::new(0, "The vault is the memory"),
            ],
            notes: Some("The nested lines are sub-points; bold works inside one.".into()),
        },
        pptx::Slide::titled("Questions?"),
    ];
    write(directory, "deck.pptx", pptx::write(&deck));

    println!("\nOpen them, or run them through LibreOffice — see the header comment.");
}

fn write(directory: &std::path::Path, name: &str, bytes: Vec<u8>) {
    let path = directory.join(name);
    std::fs::write(&path, &bytes).expect("to write the file");
    println!("{:>14}  {:>7} bytes", path.display(), bytes.len());
}
