//! The host updating itself by re-exec in place (spike T36, decision 0018), on the real adapters.
//!
//! The runtime decides and quiesces ([`HostRuntime::run`](super::HostRuntime::run) hands out a [`SwapPlan`]); this
//! module does the rest ([`hand_off`], then [`exec`]), in this order:
//! 1. hand the listener over, so every connect from now on waits in the backlog for the new image (before the conns
//!    close: a client reconnecting at once must not be accepted by this image and cut off by the exec);
//! 2. close every conn;
//! 3. write `<root>/run/handoff-<pid>.json` (`0600`);
//! 4. clear `CLOEXEC` on exactly the carried descriptors (the log's lock, the listener, each session's master and
//!    slave);
//! 5. `execv` the path the host was started from (never `current_exe()` now: the file there may have been replaced).
//!
//! The log is never dropped first: its `Drop` would unlock the description the new image inherits. If the exec
//! returns, every step is undone and the runtime resumes. A SIGTERM that arrives between the upgrade's `Accepted` and
//! the exec is dropped: the old image's signal thread queues it for a loop that never runs again, and the new image
//! installs its handlers afresh (accepted: the window is the time to write one small file).
//!
//! The new image (`shepherd host --adopt <file>`) [`read`]s and deletes the file, which must be exactly
//! `<root>/run/handoff-<its pid>.json`, a regular `0600` file of the root's owner, naming each descriptor once. It sets
//! `CLOEXEC` on the carried descriptors again, so its own children never inherit them, and adopts them in order: the
//! lock, the listener, then the PTYs, before the logs are folded.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::io::Read;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
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
    /// The path the old image execs, which the new one keeps as its startup path.
    pub exe: PathBuf,
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

    /// Why the descriptors it names can't all be ours, if they can't: each must be named once, and none can be
    /// stdin, stdout or stderr. A repeat would have one adoption close what another owns.
    fn check_fds(&self) -> Result<(), String> {
        let mut seen = BTreeSet::new();
        for fd in self.fds() {
            if fd < 3 {
                return Err(format!("it names descriptor {fd}"));
            }
            if !seen.insert(fd) {
                return Err(format!("it names descriptor {fd} twice"));
            }
        }
        Ok(())
    }
}

/// Where the handoff file of the host with pid `pid` goes.
pub fn handoff_path(root: &Path, pid: u32) -> PathBuf {
    root.join(RUN_DIR).join(format!("handoff-{pid}.json"))
}

/// A swap handed off and ready to exec: the listener handed over, the conns closed, the file written and the
/// descriptors inheritable. [`exec`] it, or [`undo`] it.
#[derive(Debug)]
pub struct HandedOff {
    pub file: HandoffFile,
    pub path: PathBuf,
}

/// Steps 1 to 4 of the swap (see the module docs), carrying `plan`'s sessions, for an exec of `exe`. On failure
/// whatever was done is undone (the conns stay closed) and the error says why.
pub fn hand_off(
    host: &mut RealHost,
    plan: SwapPlan<PtyToken>,
    root: &Path,
    exe: &Path,
) -> Result<HandedOff, String> {
    let listener = host
        .link_mut()
        .hand_over()
        .map_err(|e| format!("cannot hand the listener over: {e}"))?;
    host.close_conns();
    let file = HandoffFile {
        version: HANDOFF_VERSION,
        source: host.settings().build.clone(),
        exe: exe.to_path_buf(),
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
    let handed = HandedOff {
        path: handoff_path(root, std::process::id()),
        file,
    };
    let done = write(&handed.file, &handed.path)
        .map_err(|e| format!("cannot write {}: {e}", handed.path.display()))
        .and_then(|()| {
            handoff::set_inherited(&handed.file.fds(), true)
                .map_err(|e| format!("cannot pass the descriptors on: {e}"))
        });
    match done {
        Ok(()) => Ok(handed),
        Err(why) => {
            undo(host, &handed);
            Err(why)
        }
    }
}

/// Undoes a [`hand_off`]: the descriptors are `CLOEXEC` again, the file is gone and the listener is taken back.
pub fn undo(host: &mut RealHost, handed: &HandedOff) {
    let _ = handoff::set_inherited(&handed.file.fds(), false);
    let _ = std::fs::remove_file(&handed.path);
    if let Err(e) = host.link_mut().take_back() {
        tracing::error!(error = %e, "cannot take the listener back; no new clients can connect");
    }
}

/// The whole swap: [`hand_off`], then re-exec `exe` as `shepherd host --adopt <file>`. Returns only if that fails,
/// with why, everything undone. The caller then calls
/// [`HostRuntime::swap_failed`](super::HostRuntime::swap_failed).
pub fn exec(host: &mut RealHost, plan: SwapPlan<PtyToken>, root: &Path, exe: &Path) -> String {
    let handed = match hand_off(host, plan, root, exe) {
        Ok(handed) => handed,
        Err(why) => return why,
    };
    tracing::info!(exe = %exe.display(), sessions = handed.file.sessions.len(), "re-executing");
    let args = [
        OsStr::new("shepherd"),
        OsStr::new("host"),
        OsStr::new("--adopt"),
        handed.path.as_os_str(),
    ];
    let error = handoff::exec(exe, &args);
    undo(host, &handed);
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

/// Reads and deletes this process's handoff file, `given` (from `--adopt`), and makes every descriptor it carries
/// `CLOEXEC` again. `given` must be exactly `<root>/run/handoff-<our pid>.json` and a regular `0600` file owned by
/// the root's owner; anything else is refused and left where it is. A file that names a descriptor twice is refused.
pub fn read(root: &Path, given: &Path) -> Result<HandoffFile, String> {
    let path = handoff_path(root, std::process::id());
    if given != path {
        return Err(format!(
            "{} is not this process's handoff file, {}",
            given.display(),
            path.display()
        ));
    }
    let mut opened =
        private_fs::open_existing_private_file(&path, std::fs::OpenOptions::new().read(true))
            .map_err(|e| e.to_string())?;
    let meta = opened.metadata().map_err(|e| e.to_string())?;
    let owner = std::fs::metadata(root).map_err(|e| e.to_string())?.uid();
    let mode = meta.permissions().mode() & 0o777;
    if mode != 0o600 || meta.uid() != owner {
        return Err(format!(
            "{} has mode {mode:o} and owner {}: not a handoff file this host wrote",
            path.display(),
            meta.uid()
        ));
    }
    let mut json = Vec::new();
    let read = opened.read_to_end(&mut json);
    let _ = std::fs::remove_file(&path);
    read.map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let file: HandoffFile = serde_json::from_slice(&json)
        .map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
    if file.version != HANDOFF_VERSION {
        return Err(format!(
            "{} is handoff format {}, and this build reads {HANDOFF_VERSION}",
            path.display(),
            file.version
        ));
    }
    file.check_fds()
        .map_err(|e| format!("{} is not a usable handoff: {e}", path.display()))?;
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

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    use super::*;

    /// A handoff file naming `lock`, `listener` and one session's `master` and `slave`.
    fn file(lock: RawFd, listener: RawFd, master: RawFd, slave: RawFd) -> HandoffFile {
        HandoffFile {
            version: HANDOFF_VERSION,
            source: None,
            exe: PathBuf::from("/opt/shepherd"),
            lock,
            listener,
            sessions: vec![HandoffSession {
                id: SessionId::new("s1"),
                cols: 80,
                rows: 24,
                snapshot: String::new(),
                token: PtyToken {
                    pid: 1,
                    master,
                    slave,
                },
            }],
            translators: CarriedTranslators::default(),
        }
    }

    /// A private root with `contents` written at `path` with `mode`.
    fn root_with(contents: &[u8], mode: u32) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let root = dir.path().join("root");
        private_fs::ensure_private_dir(&root.join(RUN_DIR)).unwrap_or_else(|e| panic!("{e}"));
        let path = handoff_path(&root, std::process::id());
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&path)
            .unwrap_or_else(|e| panic!("{e}"));
        out.write_all(contents).unwrap_or_else(|e| panic!("{e}"));
        fs::set_permissions(&path, fs::Permissions::from_mode(mode))
            .unwrap_or_else(|e| panic!("{e}"));
        (dir, root)
    }

    fn json(file: &HandoffFile) -> Vec<u8> {
        serde_json::to_vec(file).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn a_path_other_than_this_processs_handoff_file_is_refused_and_left_alone() {
        let (_dir, root) = root_with(&json(&file(10, 11, 12, 13)), 0o600);
        let other = root.join(RUN_DIR).join("handoff-1.json");
        fs::copy(handoff_path(&root, std::process::id()), &other).unwrap_or_else(|e| panic!("{e}"));
        fs::set_permissions(&other, fs::Permissions::from_mode(0o600))
            .unwrap_or_else(|e| panic!("{e}"));

        assert!(read(&root, &other).is_err());
        assert!(other.exists(), "deleted a file it refused");
    }

    #[test]
    fn a_handoff_file_others_can_read_is_refused_and_left_alone() {
        let (_dir, root) = root_with(&json(&file(10, 11, 12, 13)), 0o644);
        let path = handoff_path(&root, std::process::id());
        assert!(read(&root, &path).is_err());
        assert!(path.exists(), "deleted a file it refused");
    }

    #[test]
    fn a_handoff_file_naming_a_descriptor_twice_is_refused() {
        for (lock, listener, master, slave) in
            [(10, 11, 10, 13), (10, 10, 12, 13), (10, 11, 12, 12)]
        {
            let (_dir, root) = root_with(&json(&file(lock, listener, master, slave)), 0o600);
            let path = handoff_path(&root, std::process::id());
            let err = read(&root, &path)
                .err()
                .unwrap_or_else(|| panic!("accepted"));
            assert!(err.contains("twice"), "{err}");
        }
    }

    #[test]
    fn a_handoff_file_naming_stdio_is_refused() {
        let (_dir, root) = root_with(&json(&file(10, 11, 2, 13)), 0o600);
        let path = handoff_path(&root, std::process::id());
        let err = read(&root, &path)
            .err()
            .unwrap_or_else(|| panic!("accepted"));
        assert!(err.contains("descriptor 2"), "{err}");
    }
}
