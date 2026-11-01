//! `shepherd screen` (T52): what a session's terminal shows, as text, read by its host from its own emulator
//! (`_shepherd/session/screen`). Shape in `docs/cli.md`.

use std::io::Write;

use schemars::JsonSchema;
use serde::Serialize;
use shepherd_core::display::display_safe;
use shepherd_core::wire::{ClientRequest, HostResult, SessionScreen};

use super::{Args, CliError, Drive, request, settle, target, write, write_json};

const SCREEN_USAGE: &str = "usage: shepherd screen <session> [--lines N] [--json]";

/// `{ session, cols, rows, scrollback, lines, truncated }` (docs/cli.md).
#[derive(Serialize, JsonSchema)]
pub(crate) struct ShownJson {
    session: String,
    /// The session's terminal size.
    cols: u16,
    rows: u16,
    /// How many of `lines` are scrollback, before the screen's rows.
    scrollback: u32,
    /// Scrollback (oldest first), then every row of the screen, top to bottom.
    lines: Vec<String>,
    /// Past 1 MiB of text the oldest lines are left out.
    truncated: bool,
}

/// `shepherd screen`: the session's screen, with up to `lines` lines of scrollback above it. For a human, each line
/// display-safe, without the blank rows below the last one drawn.
pub fn screen(
    drive: &mut impl Drive,
    session: &str,
    lines: Option<u32>,
    json: bool,
    out: &mut impl Write,
) -> Result<(), CliError> {
    settle(drive)?;
    let session = target(drive, session)?;
    let body = ClientRequest::SessionScreen(SessionScreen {
        session: session.id.clone(),
        lines,
    });
    let shown = match request(drive, Some(session.machine.clone()), body)? {
        HostResult::SessionScreen(shown) => shown,
        other => return Err(format!("the host answered something else: {other:?}").into()),
    };
    if json {
        let shown = ShownJson {
            session: session.id.to_string(),
            cols: shown.cols,
            rows: shown.rows,
            scrollback: shown.scrollback,
            lines: shown.lines,
            truncated: shown.truncated,
        };
        return Ok(write_json(out, &shown)?);
    }
    let drawn = shown
        .lines
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .map_or(0, |last| last + 1);
    let text: String = shown.lines[..drawn]
        .iter()
        .map(|line| {
            format!(
                "{}\n",
                display_safe(line, usize::from(shepherd_term::MAX_COLS))
            )
        })
        .collect();
    Ok(write(out, &text)?)
}

/// The session, `--lines` and `--json` of a `shepherd screen` command line.
fn screen_args(args: &[String]) -> Result<(String, Option<u32>, bool), String> {
    let parsed = Args::parse(args, &["--lines"], &["--json"]).ok_or(SCREEN_USAGE)?;
    let [session] = parsed.positional.as_slice() else {
        return Err(SCREEN_USAGE.into());
    };
    let lines = parsed
        .value("--lines")
        .map(|n| {
            n.parse::<u32>()
                .map_err(|_| format!("--lines {}: give a number of lines", super::safe(n)))
        })
        .transpose()?;
    Ok((session.clone(), lines, parsed.switch("--json")))
}

/// `shepherd screen <args>`.
pub fn screen_main(args: &[String]) -> Result<(), CliError> {
    let (session, lines, json) = screen_args(args)?;
    screen(
        &mut super::connect()?,
        &session,
        lines,
        json,
        &mut std::io::stdout(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    #[test]
    fn screen_takes_a_session_and_maybe_a_number_of_lines() {
        assert_eq!(
            screen_args(&args(&["api", "--lines", "40", "--json"])),
            Ok(("api".to_owned(), Some(40), true))
        );
        assert_eq!(
            screen_args(&args(&["api"])),
            Ok(("api".to_owned(), None, false))
        );
        assert!(screen_args(&args(&["api", "--lines", "-1"])).is_err());
        assert!(screen_args(&args(&["api", "--lines", "lots"])).is_err());
        assert!(screen_args(&args(&[])).is_err());
        assert!(screen_args(&args(&["api", "web"])).is_err());
    }
}
