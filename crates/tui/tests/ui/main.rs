//! UI tests through the headless harness ([`harness::Ui`]): a real client and renderer, driven by clicking what is
//! drawn, typing and resizing, against a scripted fake host.
//!
//! - `steps`: the harness's own steps and assertions.
//! - `sidebar`, `popups`, `finder`: point-and-click tests ported from `mouse.rs` and `finder.rs`.
//! - `invariants`: random step sequences that never panic and keep the UI's invariants.

mod finder;
mod harness;
mod invariants;
mod popups;
mod sidebar;
mod steps;
