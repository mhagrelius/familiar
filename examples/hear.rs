//! Transcribe a WAV through the same worker the application listens with.
//!
//! The one seam the widget tests cannot reach: whether a speech model is
//! actually installed, actually loads, and actually returns words. Everything
//! above it — the endpointer, the sentence reader, the window — is tested
//! against values; this is tested against a file, because the alternative is
//! finding out by pressing the shortcut.
//!
//! ```sh
//! cargo run --release --example hear -- ~/Projects/magpie/.whisper-build/whisper.cpp/samples/jfk.wav
//! ```
//!
//! It takes 16-bit mono PCM WAV at 16 kHz, which is what `pw-record` is asked
//! for and what the models want. Anything else is a `ffmpeg -ar 16000 -ac 1`
//! away.

use gtk::glib;

use familiar::ui::voice::speech::{self, Model, Speech};

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: hear <file.wav>");
        std::process::exit(2);
    };

    for model in [Model::Accurate, Model::Live] {
        match speech::model_dir(model) {
            Some(dir) => println!("{model:?}: {}", dir.display()),
            None => println!("{model:?}: not installed"),
        }
    }
    if !speech::is_installed() {
        eprintln!("\n{}", speech::MISSING);
        std::process::exit(1);
    }

    let samples = read_wav(&path);
    println!(
        "\n{path}: {} samples, {:.1}s",
        samples.len(),
        samples.len() as f64 / 16_000.0
    );

    // The worker answers on the main loop, which is where the application
    // receives it too — so this exercises the hop as well as the model.
    let main_loop = glib::MainLoop::new(None, false);
    let speech = Speech::new();
    let started = std::time::Instant::now();
    speech.transcribe(samples, {
        let main_loop = main_loop.clone();
        move |result| {
            match result {
                Ok(text) => println!("\n{:.2}s: {text}", started.elapsed().as_secs_f64()),
                Err(error) => println!("\nfailed: {error}"),
            }
            main_loop.quit();
        }
    });
    main_loop.run();
}

/// The narrowest WAV reader that can be correct for what this takes: 16-bit
/// mono at 16 kHz. Anything else is refused rather than resampled, because a
/// silently wrong sample rate reads as a bad model.
fn read_wav(path: &str) -> Vec<f32> {
    let bytes = std::fs::read(path).unwrap_or_else(|error| {
        eprintln!("{path}: {error}");
        std::process::exit(1);
    });
    if bytes.len() < 44 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        eprintln!("{path}: not a WAV file");
        std::process::exit(1);
    }

    // Walk the chunks rather than assuming a 44-byte header: a file written by
    // anything but the simplest encoder has a LIST chunk in the middle of it.
    let mut at = 12;
    let mut channels = 0u16;
    let mut rate = 0u32;
    let mut bits = 0u16;
    let mut data: &[u8] = &[];
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let size = u32::from_le_bytes([bytes[at + 4], bytes[at + 5], bytes[at + 6], bytes[at + 7]])
            as usize;
        let body = at + 8;
        let end = (body + size).min(bytes.len());
        match id {
            b"fmt " if size >= 16 => {
                channels = u16::from_le_bytes([bytes[body + 2], bytes[body + 3]]);
                rate = u32::from_le_bytes([
                    bytes[body + 4],
                    bytes[body + 5],
                    bytes[body + 6],
                    bytes[body + 7],
                ]);
                bits = u16::from_le_bytes([bytes[body + 14], bytes[body + 15]]);
            }
            b"data" => data = &bytes[body..end],
            _ => {}
        }
        at = body + size + (size % 2);
    }

    if channels != 1 || rate != 16_000 || bits != 16 {
        eprintln!(
            "{path}: {channels} channels at {rate} Hz, {bits}-bit — wanted mono 16 kHz 16-bit"
        );
        std::process::exit(1);
    }
    data.chunks_exact(2)
        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]) as f32 / 32_768.0)
        .collect()
}
