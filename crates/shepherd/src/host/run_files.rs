//! Each session's run files (`launch::RunFile`, e.g. Claude Code's generated settings), in `<run_root>/<session>/`
//! under `~/.shepherd/run`. Created through `private_fs` (decision 0010): directories `0700`, files `0600`.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use shepherd_adapters::private_fs::{self, PrivateFsError};
use shepherd_core::host::launch::RunFile;
use shepherd_core::ids::SessionId;

#[derive(Debug, thiserror::Error)]
pub enum RunFilesError {
    #[error(transparent)]
    Private(#[from] PrivateFsError),
    #[error("cannot write {path}: {source}")]
    Write { path: PathBuf, source: io::Error },
    #[error("run file name {0:?} is not a plain file name")]
    BadName(String),
}

/// The directory `session`'s run files go in.
pub fn session_dir(run_root: &Path, session: &SessionId) -> PathBuf {
    run_root.join(session.as_str())
}

/// Creates `<run_root>/<session>/` and writes `files` into it, replacing any file of the same name.
pub fn write(run_root: &Path, session: &SessionId, files: &[RunFile]) -> Result<(), RunFilesError> {
    private_fs::ensure_private_dir(run_root)?;
    let dir = session_dir(run_root, session);
    private_fs::ensure_private_dir(&dir)?;
    for file in files {
        let name = Path::new(&file.name);
        if name.file_name() != Some(name.as_os_str()) {
            return Err(RunFilesError::BadName(file.name.clone()));
        }
        let path = dir.join(name);
        let write_error = |source| RunFilesError::Write {
            path: path.clone(),
            source,
        };
        let mut out = private_fs::create_or_open_private_file(
            &path,
            OpenOptions::new().write(true).truncate(true),
        )?;
        out.write_all(&file.contents).map_err(write_error)?;
    }
    Ok(())
}

/// Removes `session`'s run directory and everything in it. A session that never had one is fine.
pub fn remove(run_root: &Path, session: &SessionId) -> io::Result<()> {
    match std::fs::remove_dir_all(session_dir(run_root, session)) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Removes every session directory in `run_root` whose name `live` doesn't claim, logging what it can't. Never
/// follows a symlink and never leaves `run_root`: a run root that isn't a real directory is left alone, an entry
/// that isn't one (a symlink, a file) is skipped, and `remove_dir_all` removes links inside a directory without
/// following them.
pub fn sweep(run_root: &Path, live: impl Fn(&str) -> bool) {
    match std::fs::symlink_metadata(run_root) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            tracing::warn!(run_root = %run_root.display(), %error, "cannot sweep run files");
            return;
        }
        Ok(meta) if !meta.is_dir() => {
            tracing::warn!(run_root = %run_root.display(), "not sweeping run files: not a directory");
            return;
        }
        Ok(_) => {}
    }
    let entries = match std::fs::read_dir(run_root) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(run_root = %run_root.display(), %error, "cannot sweep run files");
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(run_root = %run_root.display(), %error, "cannot sweep run files");
                return;
            }
        };
        let path = entry.path();
        // `DirEntry::file_type` doesn't follow a symlink.
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        let name = entry.file_name();
        if !is_dir || name.to_str().is_some_and(&live) {
            continue;
        }
        match std::fs::remove_dir_all(&path) {
            Ok(()) => tracing::info!(path = %path.display(), "swept stale run files"),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "cannot sweep run files")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str) -> RunFile {
        RunFile {
            name: name.to_owned(),
            contents: b"x".to_vec(),
        }
    }

    #[test]
    fn a_run_file_name_that_is_a_path_is_refused_and_written_nowhere() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let run_root = tmp.path().join("run");
        let session = SessionId::new("s1");
        for name in ["../x", "a/b"] {
            let result = write(&run_root, &session, &[file(name)]);
            assert!(
                matches!(&result, Err(RunFilesError::BadName(n)) if n == name),
                "{name}: {result:?}"
            );
        }
        assert!(!run_root.join("x").exists());
        assert!(!session_dir(&run_root, &session).join("a").exists());
    }

    #[test]
    fn removing_a_session_that_never_had_run_files_is_fine() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let result = remove(&tmp.path().join("run"), &SessionId::new("s1"));
        assert!(result.is_ok(), "{result:?}");
    }
}
