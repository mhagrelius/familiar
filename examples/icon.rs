//! Render the app icon to a PNG, so "is it blank?" has an answer.
//!
//! ```sh
//! cargo run --example icon -- /tmp/icon.png
//! ```
use gtk::gdk;
use gtk::prelude::*;

fn main() {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/icon.png".into());
    let source = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "data/icons/hicolor/scalable/apps/us.hagreli.Familiar.svg".into());

    gtk::init().expect("a display");
    let texture = match gdk::Texture::from_filename(&source) {
        Ok(texture) => texture,
        Err(error) => {
            eprintln!("{source}: {error}");
            std::process::exit(1);
        }
    };
    println!("{source}: {}x{}", texture.width(), texture.height());
    texture.save_to_png(&out).expect("write the png");
    println!("wrote {out}");
}
