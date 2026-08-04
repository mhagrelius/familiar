//! Ingest a PDF the way the app does, and ask the model about it.
//!
//! The composer's path runs through gio subprocesses on the main loop; this
//! runs the same `documents` functions with plain `Command`s so the whole
//! pipeline — count, extract, plan, frame, rasterise, ask — can be checked in
//! one line from a terminal.
//!
//! ```sh
//! cargo run --example pdf -- report.pdf "what was revenue, and what is on the chart?"
//! ```

use std::cell::RefCell;
use std::process::Command;
use std::rc::Rc;

use familiar::model::documents;
use familiar::model::images::Attachment;
use familiar::model::turn::TurnStream;
use familiar::model::wire::{ChatRequest, Message};
use familiar::ui::client::Client;
use gtk::glib;

fn main() {
    let mut arguments = std::env::args().skip(1);
    let path = std::path::PathBuf::from(arguments.next().expect("a pdf path"));
    let question = arguments
        .next()
        .unwrap_or_else(|| "Summarise this document. Cite page numbers.".to_string());

    let bytes = std::fs::read(&path).expect("the file");
    if !documents::is_pdf(&bytes) {
        eprintln!("that is not a PDF");
        std::process::exit(1);
    }

    let info = documents::parse_info(&run(documents::info_command(&path)));
    let extracted = documents::split_pages(&run(documents::extract_command(&path)));
    let plan = documents::plan(info.pages, &extracted);

    println!("{} pages", plan.pages.len());
    for page in &plan.pages {
        match page {
            documents::Page::Text { number, text } => {
                println!("  page {number}: text ({} chars)", text.len())
            }
            documents::Page::NeedsImage { number } => println!("  page {number}: needs an image"),
        }
    }
    if !plan.omitted.is_empty() {
        println!("  omitted: {:?}", plan.omitted);
    }

    // Render only the pages with no words.
    let temporary = tempfile::tempdir().expect("temp dir");
    let mut images = Vec::new();
    for page in &plan.to_rasterise {
        let prefix = temporary.path().join(format!("p{page}"));
        run(documents::rasterise_page_command(&path, &prefix, *page));
        if let Some(bytes) = documents::collect_page(temporary.path(), &format!("p{page}")) {
            if let Some(attachment) = Attachment::new(bytes, digest) {
                println!("  rendered page {page} ({} bytes)", attachment.bytes.len());
                images.push(attachment);
            }
        }
    }

    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let framed = documents::frame(&name, &info, &plan);
    println!("\n--- framed for the model: {} chars ---\n", framed.len());

    let asked = format!("{framed}\n\n{question}");
    let message = if images.is_empty() {
        Message::user(asked)
    } else {
        Message::user_with_images(asked, images.iter().map(Attachment::data_url).collect())
    };

    let client = Client::new("http://127.0.0.1:8080");
    let main_loop = glib::MainLoop::new(None, false);
    let stream = Rc::new(RefCell::new(TurnStream::new()));

    let _keep = client.stream(
        &ChatRequest {
            messages: vec![message],
            ..ChatRequest::new(Vec::new())
        },
        {
            let stream = stream.clone();
            move |text: &str| {
                for event in stream.borrow_mut().push(text) {
                    if let familiar::model::turn::Event::Answer(fragment) = event {
                        print!("{fragment}");
                        use std::io::Write;
                        let _ = std::io::stdout().flush();
                    }
                }
            }
        },
        {
            let stream = stream.clone();
            let main_loop = main_loop.clone();
            move |outcome| {
                if let Err(error) = &outcome {
                    eprintln!("\ntransport: {error}");
                }
                let mut borrowed = stream.borrow_mut();
                borrowed.end();
                let state = std::mem::take(&mut *borrowed).finish();
                println!("\n\n--- {} ---", state.metrics().one_line());
                main_loop.quit();
            }
        },
    );
    std::mem::forget(_keep);
    main_loop.run();
}

fn run(command: Vec<String>) -> String {
    let output = Command::new(&command[0])
        .args(&command[1..])
        .output()
        .unwrap_or_else(|error| panic!("{}: {error}", command[0]));
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn digest(bytes: &[u8]) -> String {
    glib::compute_checksum_for_bytes(glib::ChecksumType::Sha256, &glib::Bytes::from(bytes))
        .map(|sum| sum.to_string())
        .unwrap_or_default()
}
