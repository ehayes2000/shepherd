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
