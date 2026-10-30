//! The host updating itself by re-exec in place (spike T36, decision 0018), on the real adapters.
//!
//! The runtime decides and quiesces ([`HostRuntime::run`](super::HostRuntime::run) hands out a [`SwapPlan`]); this module does the rest, in
//! this order: hand the listener over (connects wait in the backlog from now on), close every conn, write `<root>/run/handoff-<pid>.json` (`0600`),
//! clear `CLOEXEC` on exactly the carried descriptors (the log's lock, the listener, each session's master and
//! slave), and `execv` the path the host was started from (never `current_exe()` now: the file there may have been
//! replaced). The log is never dropped first: its `Drop` would unlock the description the new image inherits. If the
//! exec returns, every step is undone and the runtime resumes.
//!
//! The new image (`shepherd host --adopt <file>`) reads and deletes the file, sets `CLOEXEC` on the carried
//! descriptors again (so its own children never inherit them), and adopts them in order: the lock, the listener, then
//! the PTYs, before the logs are folded.

use std::ffi::OsStr;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use shepherd_adapters::PtyToken;
use shepherd_adapters::handoff;
use shepherd_adapters::private_fs;
use shepherd_core::agents::CarriedTranslators;
use shepherd_core::host::upgrade::HANDOFF_VERSION;
use shepherd_core::ids::SessionId;
use shepherd_core::wire::{BuildId, base64};

use super::{CarriedSession, RUN_DIR, RealHost, SwapPlan};

/// What the old image tells the new one, as JSON. Versioned: a build lists the versions it reads in
/// `handoff-check`, and the preflight checks this one is among them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandoffFile {
    pub version: u32,
    /// The build that wrote it.
    pub source: Option<BuildId>,
    /// The descriptor holding the log's lock.
    pub lock: RawFd,
    /// The listening socket's descriptor.
    pub listener: RawFd,
    pub sessions: Vec<HandoffSession>,
    pub translators: CarriedTranslators,
}

/// One carried session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffSession {
    pub id: SessionId,
    pub cols: u16,
    pub rows: u16,
    /// Its screen's `snapshot_ansi`, base64.
    pub snapshot: String,
    pub token: PtyToken,
}

impl HandoffFile {
    /// Every descriptor the exec must carry.
    pub fn fds(&self) -> Vec<RawFd> {
        let mut fds = vec![self.lock, self.listener];
        fds.extend(self.sessions.iter().flat_map(|s| s.token.fds()));
        fds
    }

    /// The plan the runtime adopts from. A snapshot that isn't base64 is dropped (the screen starts blank).
    pub fn plan(&self) -> SwapPlan<PtyToken> {
        let sessions = self
            .sessions
            .iter()
            .map(|s| CarriedSession {
                id: s.id.clone(),
                cols: s.cols,
                rows: s.rows,
                snapshot: base64::decode(&s.snapshot).unwrap_or_default(),
                token: s.token,
            })
            .collect();
        SwapPlan {
            sessions,
            translators: self.translators.clone(),
        }
    }
}

/// Where the handoff file of the host with pid `pid` goes.
pub fn handoff_path(root: &Path, pid: u32) -> PathBuf {
    root.join(RUN_DIR).join(format!("handoff-{pid}.json"))
}

/// Re-execs `exe` as `shepherd host --adopt <file>`, carrying `plan`'s sessions. Returns only if that fails, with
/// why, after undoing everything: the listener is taken back, the descriptors are `CLOEXEC` again and the file is
/// gone. The caller then calls [`HostRuntime::swap_failed`](super::HostRuntime::swap_failed).
pub fn exec(host: &mut RealHost, plan: SwapPlan<PtyToken>, root: &Path, exe: &Path) -> String {
    let listener = match host.link_mut().hand_over() {
        Ok(fd) => fd,
        Err(e) => return format!("cannot hand the listener over: {e}"),
    };
    host.close_conns();
    let file = HandoffFile {
        version: HANDOFF_VERSION,
        source: host.settings().build.clone(),
        lock: host.log().lock_fd(),
        listener,
        sessions: plan
            .sessions
            .into_iter()
            .map(|s| HandoffSession {
                id: s.id,
                cols: s.cols,
                rows: s.rows,
                snapshot: base64::encode(&s.snapshot),
                token: s.token,
            })
            .collect(),
        translators: plan.translators,
    };
    let path = handoff_path(root, std::process::id());
    let why = write_and_exec(&file, &path, exe);
    let _ = handoff::set_inherited(&file.fds(), false);
    let _ = std::fs::remove_file(&path);
    if let Err(e) = host.link_mut().take_back() {
        tracing::error!(error = %e, "cannot take the listener back; no new clients can connect");
    }
    why
}

fn write_and_exec(file: &HandoffFile, path: &Path, exe: &Path) -> String {
    if let Err(e) = write(file, path) {
        return format!("cannot write {}: {e}", path.display());
    }
    if let Err(e) = handoff::set_inherited(&file.fds(), true) {
        return format!("cannot pass the descriptors on: {e}");
    }
    tracing::info!(exe = %exe.display(), sessions = file.sessions.len(), "re-executing");
    let args = [
        OsStr::new("shepherd"),
        OsStr::new("host"),
        OsStr::new("--adopt"),
        path.as_os_str(),
    ];
    let error = handoff::exec(exe, &args);
    format!("cannot run {}: {error}", exe.display())
}

fn write(file: &HandoffFile, path: &Path) -> Result<(), String> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        private_fs::ensure_private_dir(dir).map_err(|e| e.to_string())?;
    }
    let _ = std::fs::remove_file(path);
    let mut out =
        private_fs::create_or_open_private_file(path, std::fs::OpenOptions::new().write(true))
            .map_err(|e| e.to_string())?;
    let json = serde_json::to_vec(file).map_err(|e| e.to_string())?;
    out.write_all(&json).map_err(|e| e.to_string())
}

/// Reads and deletes the handoff file at `path`, and makes every descriptor it carries `CLOEXEC` again.
pub fn read(path: &Path) -> Result<HandoffFile, String> {
    let read = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()));
    let _ = std::fs::remove_file(path);
    let file: HandoffFile = serde_json::from_slice(&read?)
        .map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
    if file.version != HANDOFF_VERSION {
        return Err(format!(
            "{} is handoff format {}, and this build reads {HANDOFF_VERSION}",
            path.display(),
            file.version
        ));
    }
    handoff::set_inherited(&file.fds(), false)
        .map_err(|e| format!("a carried descriptor is not open: {e}"))?;
    Ok(file)
}

/// Takes ownership of a carried descriptor.
///
/// # Safety
/// `fd` must be open and owned by nothing else in this process: true of what the old image carried, once.
pub unsafe fn own(fd: RawFd) -> OwnedFd {
    // SAFETY: as the caller promises.
    unsafe { OwnedFd::from_raw_fd(fd) }
}
