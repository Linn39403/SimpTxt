//! A small file. A clear view. Room to think.
//!
//! SimpTxt keeps the essentials close:
//! open, edit, search, and save without the clutter.

use std::time::Instant;

#[derive(Debug)]
struct Document {
    name: String,
    lines: usize,
    modified: bool,
}

impl Document {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            lines: 1,
            modified: false,
        }
    }

    fn describe(&self) -> String {
        let marker = if self.modified { "*" } else { "" };
        format!("{}{marker} · {} lines", self.name, self.lines)
    }
}

fn main() {
    let started = Instant::now();
    let document = Document::new("welcome.rs");

    // Plain text, syntax color, and nothing in the way.
    println!("Hello from SimpTxt!");
    println!("{}", document.describe());
    println!("Ready in {:?}", started.elapsed());
}
