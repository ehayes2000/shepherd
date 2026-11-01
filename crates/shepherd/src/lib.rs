//! Host and client runtimes. They are generic over ports so scenario tests can run them with fakes.

pub mod archive;
pub mod archive_cli;
pub mod build;
pub mod cli;
pub mod client;
pub mod hook;
pub mod host;
pub mod mcp;
pub mod peers;
pub mod screen_guard;
pub mod screen_text;
pub mod terminal;

/// The most grapheme clusters [`stderr_safe`] keeps of one line.
pub const STDERR_LINE_MAX: usize = 4096;

/// `text` made display-safe for printing to a terminal, line by line: hostile text in an error (a path, an argument)
/// can't drive the terminal, and the error's own line breaks stay.
pub fn stderr_safe(text: &str) -> String {
    text.split('\n')
        .map(|line| shepherd_core::display::display_safe(line, STDERR_LINE_MAX).into_string())
        .collect::<Vec<_>>()
        .join("\n")
}
