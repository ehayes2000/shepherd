//! UI tests through the headless harness ([`harness::Ui`]): a real client and renderer, driven by clicking what is
//! drawn, typing and resizing, against a scripted fake host.
//!
//! - `steps`: the harness's own steps and assertions.
//! - `sidebar`, `popups`, `finder`: point-and-click tests ported from `mouse.rs` and `finder.rs`.
//! - `layout`: the host's layout drawn: pages, split panes, focus, dividers and the pane menu (T49).
//! - `status`: the status page's table (T57).
//! - `status_feed`: the status page's events feed and message line (T59).
//! - `invariants`: random step sequences that never panic and keep the UI's invariants.

mod finder;
mod harness;
mod invariants;
mod layout;
mod popups;
mod sidebar;
mod status;
mod status_feed;
mod steps;
mod subagents;
