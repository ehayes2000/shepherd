//! The project manager's page (T60): `ctrl-b p` ([`Intent::OpenPm`](super::Intent::OpenPm)), or the pane menu's "Project
//! manager", shows a page with the PM session on the left and the PM panel ([`ViewKind::PmPanel`]) on the right.
//!
//! The PM is an ordinary session of this machine's host started with the built-in [`PM_PROFILE`] (Claude Code with
//! shepherd's MCP server, see [`crate::host::launch`]): its own row in the sidebar, archived and resumable like any other.
//! Which session is the PM is the profile its `session/created` recorded, so it is found again after a restart of the
//! client or the host: the running one, else the newest (an exited PM is shown, to be resumed, not replaced). A peer's
//! sessions are never this window's PM.
//!
//! Opening the page:
//! - A page with the PM panel exists: its page is selected (if it isn't the current one) and the pane showing the PM is
//!   focused. If no pane there shows the PM (it closed, so the host emptied its pane), the PM goes into the page's first
//!   empty pane.
//! - Otherwise, with a PM, the host is asked for a new page, [`PM_PAGE`], split side by side: the PM, then the panel.
//! - With no PM, one is started; once it is listed, the page is opened as above. A refused start stops the wait.

use super::{Client, ClientCommand, Focus};
use crate::acpp::RequestId;
use crate::config::PM_PROFILE;
use crate::ids::SessionId;
use crate::layout::{Content, LayoutOp, Pane, PaneId, Ratio, SplitDir, ViewKind};
use crate::wire::{ClientMessage, ClientRequest, SessionNew, UiApply};

use super::status::find_leaf;

/// What a page opened for the project manager is called.
pub const PM_PAGE: &str = "pm";

/// How much of the PM's page its session takes, in thousandths: the panel gets the rest.
const PM_RATIO: u16 = 600;

impl Client {
    /// The project manager: this machine's host's session that was started with the [`PM_PROFILE`], the running one,
    /// else the newest. `None` with none listed.
    pub fn pm_session(&self) -> Option<&SessionId> {
        let mut pms = self.sessions.iter().filter(|s| {
            s.profile.as_deref() == Some(PM_PROFILE)
                && self.homes.get(&s.id) == Some(&super::At::Local)
        });
        let newest = pms.clone().next();
        pms.find(|s| !s.exited).or(newest).map(|s| &s.id)
    }

    /// Shows the project manager's page (see the module docs).
    pub(super) fn open_pm(&mut self) -> Vec<ClientCommand> {
        let pm = self.pm_session().cloned();
        let layout = self.host_layout();
        let mut commands = Vec::new();
        self.focus = Focus::Terminal;
        match find_leaf(&layout, |c| *c == Content::View(ViewKind::PmPanel)) {
            Some((page, panel)) => {
                let leaves = layout
                    .page(page)
                    .map(|p| leaves(&p.root))
                    .unwrap_or_default();
                let showing = pm.as_ref().and_then(|pm| {
                    leaves
                        .iter()
                        .find(|(_, c)| *c == Content::session(pm.clone()))
                        .map(|(id, _)| *id)
                });
                let empty = leaves
                    .iter()
                    .find(|(_, c)| *c == Content::Empty)
                    .map(|(id, _)| *id);
                self.focused = Some(match (showing, empty, &pm) {
                    (Some(pane), _, _) => pane,
                    (None, Some(pane), Some(pm)) => {
                        commands.extend(self.user_request(ClientRequest::UiApply(UiApply {
                            op: LayoutOp::Put {
                                pane,
                                content: Content::session(pm.clone()),
                            },
                        })));
                        pane
                    }
                    (None, Some(pane), None) => pane,
                    (None, None, _) => panel,
                });
                if page != layout.current() {
                    commands.extend(self.user_request(ClientRequest::UiApply(UiApply {
                        op: LayoutOp::PageSelect { page },
                    })));
                }
            }
            None => {
                if let Some(pm) = &pm {
                    commands.extend(self.user_request(ClientRequest::UiApply(UiApply {
                        op: LayoutOp::PageNewSplit {
                            name: Some(PM_PAGE.to_owned()),
                            dir: SplitDir::Horizontal,
                            ratio: Ratio::from_permille(PM_RATIO),
                            a: Content::session(pm.clone()),
                            b: Content::View(ViewKind::PmPanel),
                        },
                    })));
                }
            }
        }
        if pm.is_none() && self.pm_wanted.is_none() {
            commands.extend(self.start_pm());
        }
        commands
    }

    /// Asks this machine's host for a PM session, and waits for it to be listed.
    fn start_pm(&mut self) -> Vec<ClientCommand> {
        let new = ClientRequest::SessionNew(SessionNew {
            profile: Some(PM_PROFILE.to_owned()),
            size: self.view_size(),
            ..SessionNew::default()
        });
        let commands = self.user_request(new);
        self.pm_wanted = commands.iter().find_map(|c| match c {
            ClientCommand::Send(ClientMessage::Request(r))
                if matches!(r.body, ClientRequest::SessionNew(_)) =>
            {
                Some(r.id.clone())
            }
            _ => None,
        });
        commands
    }

    /// Once a PM started for the page is listed, opens the page.
    pub(super) fn pm_listed(&mut self) -> Vec<ClientCommand> {
        if self.pm_wanted.is_none() || self.pm_session().is_none() {
            return Vec::new();
        }
        self.pm_wanted = None;
        self.open_pm()
    }

    /// Whether `id` is the request that started a PM for the page.
    pub(super) fn pm_asked(&self, id: &RequestId) -> bool {
        self.pm_wanted.as_ref() == Some(id)
    }

    /// The host refused to start the PM: the page is no longer waiting for it.
    pub(super) fn pm_refused(&mut self, why: String) {
        self.pm_wanted = None;
        self.set_notice(format!("can't start the project manager: {why}"));
    }
}

/// The leaves of `pane`, in order, with what each shows.
fn leaves(pane: &Pane) -> Vec<(PaneId, Content)> {
    match pane {
        Pane::Leaf { id, content } => vec![(*id, content.clone())],
        Pane::Split { a, b, .. } => {
            let mut out = leaves(a);
            out.extend(leaves(b));
            out
        }
    }
}
