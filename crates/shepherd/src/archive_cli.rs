//! `shepherd archive list | show <session> | find <commit|branch|#pr> | restore <session> [--cwd <dir>] | verify`
//! (T43): reads this machine's conversation archive (`~/.shepherd/archive`, decision 0020). Every command takes
//! `--json`, whose shapes `docs/archive-cli.md` documents; the human output is display-safe.
//!
//! The archive is opened read-only ([`Archive::open_read_only`]), so these work while the host writes it, never make
//! it wait, and never change the index. `restore` writes only the agent's transcript files, through the archive's own
//! restore, which never overwrites a different file.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::Serialize;
use shepherd_adapters::GitWorktrees;
use shepherd_archive::{
    AgentKind, Archive, ArchiveError, BlobError, Branch, CommitHash, CommitHit, PrNumber, RepoId,
    Session, SessionId, Snapshot, VerifyReport,
};
use shepherd_core::display::display_safe;
use shepherd_core::host::capture::AgentHomes;
use shepherd_core::ports::{self, RepoRef, WorktreeError, Worktrees};

use crate::host::{ARCHIVE_DIR, WORKTREES_DIR};

const USAGE: &str = "usage: shepherd archive list | show <session> | find <commit|branch|#pr> | restore <session> [--cwd <dir>] | verify  (each takes --json)";

/// The most grapheme clusters of one field printed.
const SHOWN_MAX: usize = 512;

/// What the commands need from the world, so `main` is the only place that reads the environment.
pub struct Context {
    /// `~/.shepherd`.
    pub root: PathBuf,
    /// Where `find` looks for the repo, and what a relative `--cwd` is relative to.
    pub cwd: PathBuf,
    pub agent_homes: AgentHomes,
}

/// `shepherd archive <args>`.
pub fn main(args: &[String]) -> Result<(), String> {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .ok_or("HOME is not set")?;
    let home = PathBuf::from(home);
    let cwd = std::env::current_dir().map_err(|e| format!("the current directory: {e}"))?;
    let context = Context {
        root: home.join(".shepherd"),
        cwd,
        agent_homes: crate::host::agent_homes_from_env(&home),
    };
    run(&context, args, &mut std::io::stdout())
}

/// A parsed command line.
#[derive(Debug, PartialEq, Eq)]
enum Request {
    List,
    Show(String),
    Find(String),
    Restore {
        session: String,
        cwd: Option<String>,
    },
    Verify,
}

fn usage() -> String {
    USAGE.to_owned()
}

fn parse(args: &[String]) -> Result<(Request, bool), String> {
    let mut json = false;
    let mut cwd = None;
    let mut positional = Vec::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--cwd" => cwd = Some(rest.next().ok_or_else(usage)?.clone()),
            flag if flag.starts_with("--cwd=") => cwd = Some(flag["--cwd=".len()..].to_owned()),
            flag if flag.starts_with("--") => return Err(usage()),
            _ => positional.push(arg.clone()),
        }
    }
    let mut positional = positional.into_iter();
    let command = positional.next().ok_or_else(usage)?;
    let mut one = || positional.next().ok_or_else(usage);
    let request = match command.as_str() {
        "list" => Request::List,
        "show" => Request::Show(one()?),
        "find" => Request::Find(one()?),
        "restore" => Request::Restore {
            session: one()?,
            cwd: cwd.take(),
        },
        "verify" => Request::Verify,
        _ => return Err(usage()),
    };
    if positional.next().is_some() || cwd.is_some() {
        return Err(usage());
    }
    Ok((request, json))
}

/// Runs `shepherd archive <args>`, writing its answer to `out`. A failure (and `verify` finding problems) is an
/// error, which `main` prints to stderr; `verify --json` has written its report first.
pub fn run(context: &Context, args: &[String], out: &mut impl Write) -> Result<(), String> {
    let (request, json) = parse(args)?;
    let root = context.root.join(ARCHIVE_DIR);
    let archive = match Archive::open_read_only(&root) {
        Ok(archive) => Some(archive),
        Err(ArchiveError::NoArchive(_)) => None,
        Err(e) => return Err(e.to_string()),
    };
    match request {
        Request::List => list(archive.as_ref(), json, out),
        Request::Show(id) => show(need(archive.as_ref(), &root)?, &id, json, out),
        Request::Find(query) => find(context, archive.as_ref(), &query, json, out),
        Request::Restore { session, cwd } => restore(
            context,
            need(archive.as_ref(), &root)?,
            &session,
            cwd,
            json,
            out,
        ),
        Request::Verify => verify(need(archive.as_ref(), &root)?, json, out),
    }
}

fn need<'a>(archive: Option<&'a Archive>, root: &Path) -> Result<&'a Archive, String> {
    archive.ok_or_else(|| ArchiveError::NoArchive(root.to_owned()).to_string())
}

fn write(out: &mut impl Write, text: &str) -> Result<(), String> {
    out.write_all(text.as_bytes())
        .and_then(|()| out.flush())
        .map_err(|e| e.to_string())
}

fn write_json(out: &mut impl Write, value: &impl Serialize) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    write(out, &format!("{text}\n"))
}

fn safe(text: &str) -> String {
    display_safe(text, SHOWN_MAX).into_string()
}

fn safe_path(path: &Path) -> String {
    safe(&path.to_string_lossy())
}

fn or_dash(text: Option<&str>) -> String {
    text.map_or_else(|| "-".to_owned(), safe)
}

fn session_id(text: &str) -> Result<SessionId, String> {
    SessionId::from_str(text).map_err(|e| e.to_string())
}

fn session(archive: &Archive, id: &str) -> Result<Session, String> {
    let id = session_id(id)?;
    archive
        .all_sessions()
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|s| s.id == id)
        .ok_or_else(|| ArchiveError::UnknownSession(id).to_string())
}

// ---- JSON shapes (docs/archive-cli.md) ----

#[derive(Serialize)]
struct SessionJson {
    id: String,
    repo: String,
    machine: String,
    agent: &'static str,
    agent_session_id: String,
    name: Option<String>,
    branch: Option<String>,
    pr: Option<u32>,
    base_commit: Option<String>,
    cwd: String,
    created_at: String,
    archived_at: Option<String>,
}

impl From<&Session> for SessionJson {
    fn from(s: &Session) -> Self {
        Self {
            id: s.id.to_string(),
            repo: s.repo.to_string(),
            machine: s.machine.to_string(),
            agent: s.agent.as_str(),
            agent_session_id: s.agent_session_id.to_string(),
            name: s.name.clone(),
            branch: s.branch.as_ref().map(ToString::to_string),
            pr: s.pr.map(PrNumber::get),
            base_commit: s.base_commit.as_ref().map(ToString::to_string),
            cwd: s.cwd.to_string_lossy().into_owned(),
            created_at: s.created_at.to_string(),
            archived_at: s.archived_at.map(|t| t.to_string()),
        }
    }
}

#[derive(Serialize)]
struct SnapshotJson {
    id: i64,
    captured_at: String,
    reason: &'static str,
    head_commit: Option<String>,
    files: Vec<FileJson>,
}

#[derive(Serialize)]
struct FileJson {
    role: &'static str,
    original_path: String,
    blob: String,
    bytes: u64,
}

impl From<&Snapshot> for SnapshotJson {
    fn from(s: &Snapshot) -> Self {
        Self {
            id: s.id.get(),
            captured_at: s.captured_at.to_string(),
            reason: s.reason.as_str(),
            head_commit: s.head_commit.as_ref().map(ToString::to_string),
            files: s
                .files
                .iter()
                .map(|f| FileJson {
                    role: f.role.as_str(),
                    original_path: f.original_path.to_string_lossy().into_owned(),
                    blob: f.blob.to_string(),
                    bytes: f.bytes,
                })
                .collect(),
        }
    }
}

// ---- list ----

fn list(archive: Option<&Archive>, json: bool, out: &mut impl Write) -> Result<(), String> {
    let sessions = match archive {
        Some(archive) => archive.all_sessions().map_err(|e| e.to_string())?,
        None => Vec::new(),
    };
    if json {
        #[derive(Serialize)]
        struct List {
            sessions: Vec<SessionJson>,
        }
        return write_json(
            out,
            &List {
                sessions: sessions.iter().map(SessionJson::from).collect(),
            },
        );
    }
    if sessions.is_empty() {
        return write(out, "No archived conversations yet.\n");
    }
    let text: String = sessions
        .iter()
        .map(|s| format!("{}\n", session_line(s)))
        .collect();
    write(out, &text)
}

/// `<id>  <agent>  <created>  <branch>  <name>`.
fn session_line(s: &Session) -> String {
    format!(
        "{}  {}  {}  {}  {}",
        safe(s.id.as_str()),
        s.agent,
        s.created_at,
        or_dash(s.branch.as_ref().map(Branch::as_str)),
        or_dash(s.name.as_deref()),
    )
}

// ---- show ----

fn show(archive: &Archive, id: &str, json: bool, out: &mut impl Write) -> Result<(), String> {
    let s = session(archive, id)?;
    let snapshots = archive.snapshots(&s.id).map_err(|e| e.to_string())?;
    if json {
        #[derive(Serialize)]
        struct Show {
            session: SessionJson,
            snapshots: Vec<SnapshotJson>,
        }
        return write_json(
            out,
            &Show {
                session: SessionJson::from(&s),
                snapshots: snapshots.iter().map(SnapshotJson::from).collect(),
            },
        );
    }
    let mut text = format!(
        "Session {}\n  agent:    {} (its session {})\n  name:     {}\n  branch:   {}\n  pr:       {}\n  repo:     {}\n  machine:  {}\n  worktree: {}\n  base:     {}\n  created:  {}\n  archived: {}\n",
        safe(s.id.as_str()),
        s.agent,
        safe(s.agent_session_id.as_str()),
        or_dash(s.name.as_deref()),
        or_dash(s.branch.as_ref().map(Branch::as_str)),
        s.pr.map_or_else(|| "-".to_owned(), |n| format!("#{n}")),
        s.repo,
        safe(s.machine.as_str()),
        safe_path(&s.cwd),
        s.base_commit
            .as_ref()
            .map_or_else(|| "-".to_owned(), ToString::to_string),
        s.created_at,
        s.archived_at
            .map_or_else(|| "-".to_owned(), |t| t.to_string()),
    );
    if snapshots.is_empty() {
        text.push_str("No snapshots.\n");
    } else {
        text.push_str("Snapshots, oldest first:\n");
        for snap in &snapshots {
            let bytes: u64 = snap.files.iter().map(|f| f.bytes).sum();
            text.push_str(&format!(
                "  #{}  {}  {}  head {}  {} files, {bytes} bytes\n",
                snap.id,
                snap.captured_at,
                snap.reason,
                snap.head_commit
                    .as_ref()
                    .map_or_else(|| "-".to_owned(), ToString::to_string),
                snap.files.len(),
            ));
        }
    }
    write(out, &text)
}

// ---- find ----

/// How a session was found.
#[derive(Clone, Copy, PartialEq, Eq)]
enum How {
    Commit,
    PatchId,
    Branch,
    Pr,
}

impl How {
    fn as_str(self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::PatchId => "patch_id",
            Self::Branch => "branch",
            Self::Pr => "pr",
        }
    }
}

struct Match {
    how: How,
    hit: Option<CommitHit>,
    session: Session,
}

#[derive(Serialize)]
struct MatchJson {
    how: &'static str,
    commit: Option<String>,
    patch_id: Option<String>,
    first_snapshot: Option<i64>,
    session: SessionJson,
}

/// The repo `cwd` is in, as the archive names it (its root commit), or `None` outside any repo.
fn repo_here(git: &GitWorktrees, here: &RepoRef) -> Result<Option<RepoId>, String> {
    match git.root_commit(here) {
        Ok(root) => Ok(Some(RepoId::from_root_commit(archive_commit(
            root.commit(),
        )?))),
        Err(WorktreeError::NotARepo(_)) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

fn archive_commit(c: &ports::CommitHash) -> Result<CommitHash, String> {
    CommitHash::from_str(c.as_str()).map_err(|e| e.to_string())
}

/// `#N` is a PR in the current repo. Anything else is resolved by git in the current repo (an abbreviated hash, a
/// branch, `HEAD`, ...), or taken as a full hash outside one, and looked up by that exact commit, then by its
/// patch-id; a name that is a valid branch name is also looked up as a branch of the current repo.
fn find(
    context: &Context,
    archive: Option<&Archive>,
    query: &str,
    json: bool,
    out: &mut impl Write,
) -> Result<(), String> {
    let git = GitWorktrees::new(context.root.join(WORKTREES_DIR));
    let here = RepoRef::new(&context.cwd);
    let repo = repo_here(&git, &here)?;
    let matches = match archive {
        None => Vec::new(),
        Some(archive) => lookup(archive, &git, &here, repo.as_ref(), query)?,
    };
    if json {
        #[derive(Serialize)]
        struct Find<'a> {
            query: &'a str,
            matches: Vec<MatchJson>,
        }
        return write_json(
            out,
            &Find {
                query,
                matches: matches
                    .iter()
                    .map(|m| MatchJson {
                        how: m.how.as_str(),
                        commit: m.hit.as_ref().map(|h| h.commit.to_string()),
                        patch_id: m
                            .hit
                            .as_ref()
                            .and_then(|h| h.patch_id.as_ref())
                            .map(ToString::to_string),
                        first_snapshot: m.hit.as_ref().map(|h| h.first_snapshot.get()),
                        session: SessionJson::from(&m.session),
                    })
                    .collect(),
            },
        );
    }
    if matches.is_empty() {
        return write(
            out,
            &format!("No archived conversation matches {}.\n", safe(query)),
        );
    }
    let text: String = matches
        .iter()
        .map(|m| {
            let by = match (m.how, &m.hit) {
                (How::Commit, Some(hit)) => format!("by commit {}", hit.commit),
                (How::PatchId, Some(hit)) => format!("by patch-id, as commit {}", hit.commit),
                (How::Pr, _) => format!(
                    "by PR #{}",
                    m.session.pr.map_or_else(String::new, |n| n.to_string())
                ),
                _ => format!(
                    "by branch {}",
                    or_dash(m.session.branch.as_ref().map(Branch::as_str))
                ),
            };
            format!("{}  ({by})\n", session_line(&m.session))
        })
        .collect();
    write(out, &text)
}

fn lookup(
    archive: &Archive,
    git: &GitWorktrees,
    here: &RepoRef,
    repo: Option<&RepoId>,
    query: &str,
) -> Result<Vec<Match>, String> {
    let err = |e: ArchiveError| e.to_string();
    let sessions = archive.all_sessions().map_err(err)?;
    let session_of = |id: &SessionId| sessions.iter().find(|s| s.id == *id).cloned();
    if let Some(number) = query.strip_prefix('#') {
        let pr = number
            .parse()
            .map(PrNumber::new)
            .map_err(|_| format!("{} is not a pull request number", safe(query)))?;
        let repo =
            repo.ok_or("finding a pull request needs a repo: run it in the repo's checkout")?;
        return Ok(archive
            .find_by_pr(repo, pr)
            .map_err(err)?
            .into_iter()
            .map(|session| Match {
                how: How::Pr,
                hit: None,
                session,
            })
            .collect());
    }
    let commit = match repo {
        Some(_) => git
            .resolve_commit(here, query)
            .map_err(|e| e.to_string())?
            .map(|c| archive_commit(&c))
            .transpose()?,
        None => None,
    }
    .or_else(|| CommitHash::from_str(query).ok());
    let mut matches = Vec::new();
    if let Some(commit) = &commit {
        let mut hits = archive.find_by_commit(commit).map_err(err)?;
        let mut how = How::Commit;
        if hits.is_empty() && repo.is_some() {
            let core = ports::CommitHash::parse(commit.as_str()).map_err(|e| e.to_string())?;
            // Only a commit this repo has has a patch-id here; a full hash it lacks has no patch to compare.
            if let Ok(Some(patch)) = git.patch_id(here, &core) {
                let patch = shepherd_archive::PatchId::from_str(patch.as_str())
                    .map_err(|e| e.to_string())?;
                hits = archive.find_by_patch_id(&patch).map_err(err)?;
                how = How::PatchId;
            }
        }
        for hit in hits {
            if let Some(session) = session_of(&hit.session) {
                matches.push(Match {
                    how,
                    hit: Some(hit),
                    session,
                });
            }
        }
    }
    if let (Some(repo), Ok(branch)) = (repo, Branch::from_str(query)) {
        let listed: BTreeSet<SessionId> = matches.iter().map(|m| m.session.id.clone()).collect();
        for session in archive.find_by_branch(repo, &branch).map_err(err)? {
            if !listed.contains(&session.id) {
                matches.push(Match {
                    how: How::Branch,
                    hit: None,
                    session,
                });
            }
        }
    }
    Ok(matches)
}

// ---- restore ----

/// How to resume `agent`'s session `id`: the command, as argv.
fn resume_command(agent: AgentKind, id: &str) -> Vec<String> {
    match agent {
        AgentKind::Claude => vec!["claude".into(), "--resume".into(), id.into()],
        AgentKind::Codex => vec!["codex".into(), "resume".into(), id.into()],
    }
}

fn restore(
    context: &Context,
    archive: &Archive,
    id: &str,
    cwd: Option<String>,
    json: bool,
    out: &mut impl Write,
) -> Result<(), String> {
    let s = session(archive, id)?;
    let target = match cwd {
        Some(dir) => std::path::absolute(context.cwd.join(dir)).map_err(|e| e.to_string())?,
        None if s.cwd.is_dir() => s.cwd.clone(),
        None => {
            return Err(format!(
                "the session's worktree {} is gone: pass --cwd <dir> to restore it for another directory",
                safe_path(&s.cwd)
            ));
        }
    };
    let (home, variable) = match s.agent {
        AgentKind::Claude => (&context.agent_homes.claude, "CLAUDE_CONFIG_DIR"),
        AgentKind::Codex => (&context.agent_homes.codex, "CODEX_HOME"),
    };
    if !home.is_dir() {
        return Err(format!(
            "{}'s home {} doesn't exist: run it once, or set {variable} if it lives elsewhere",
            s.agent,
            safe_path(home)
        ));
    }
    let resume = archive
        .restore(&s.id, &target, home)
        .map_err(|e| e.to_string())?;
    let command = resume_command(resume.agent, resume.agent_session_id.as_str());
    if json {
        #[derive(Serialize)]
        struct Restored {
            session: String,
            agent: &'static str,
            agent_session_id: String,
            cwd: String,
            command: Vec<String>,
        }
        return write_json(
            out,
            &Restored {
                session: s.id.to_string(),
                agent: resume.agent.as_str(),
                agent_session_id: resume.agent_session_id.to_string(),
                cwd: resume.cwd.to_string_lossy().into_owned(),
                command,
            },
        );
    }
    let command: Vec<String> = command.iter().map(|a| safe(a)).collect();
    write(
        out,
        &format!(
            "Restored session {} ({}) for {}.\nResume it with:\n  cd {}\n  {}\n",
            safe(s.id.as_str()),
            resume.agent,
            safe_path(&resume.cwd),
            safe_path(&resume.cwd),
            command.join(" "),
        ),
    )
}

// ---- verify ----

#[derive(Serialize)]
struct BadRowJson {
    snapshot: i64,
    role: &'static str,
    original_path: String,
    blob: String,
    problem: &'static str,
    error: String,
}

fn problem(e: &BlobError) -> &'static str {
    match e {
        BlobError::Missing { .. } => "missing",
        BlobError::Corrupt { .. } => "corrupt",
        BlobError::TooLarge { .. } => "too_large",
        _ => "unreadable",
    }
}

fn verify(archive: &Archive, json: bool, out: &mut impl Write) -> Result<(), String> {
    let report: VerifyReport = archive.verify().map_err(|e| e.to_string())?;
    let problems = report.bad_rows.len() + report.orphans.len();
    if json {
        #[derive(Serialize)]
        struct Verify {
            ok: bool,
            bad_rows: Vec<BadRowJson>,
            orphans: Vec<String>,
        }
        write_json(
            out,
            &Verify {
                ok: problems == 0,
                bad_rows: report
                    .bad_rows
                    .iter()
                    .map(|r| BadRowJson {
                        snapshot: r.snapshot.get(),
                        role: r.file.role.as_str(),
                        original_path: r.file.original_path.to_string_lossy().into_owned(),
                        blob: r.file.blob.to_string(),
                        problem: problem(&r.error),
                        error: r.error.to_string(),
                    })
                    .collect(),
                orphans: report.orphans.iter().map(ToString::to_string).collect(),
            },
        )?;
    } else if problems == 0 {
        write(out, "The archive is healthy.\n")?;
    } else {
        let mut text = String::new();
        for r in &report.bad_rows {
            text.push_str(&format!(
                "snapshot #{} {} {}: {} ({})\n",
                r.snapshot,
                r.file.role,
                safe_path(&r.file.original_path),
                problem(&r.error),
                safe(&r.error.to_string()),
            ));
        }
        for orphan in &report.orphans {
            text.push_str(&format!(
                "orphan blob {orphan} (no snapshot refers to it)\n"
            ));
        }
        write(out, &text)?;
    }
    if problems == 0 {
        Ok(())
    } else {
        Err(format!(
            "the archive has {problems} problem(s); nothing was changed"
        ))
    }
}
