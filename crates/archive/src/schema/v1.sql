-- Version 1: the proposal's sketch (docs/proposals/conversation-archive.md), untyped and unconstrained.
-- Times are microseconds since the Unix epoch.
CREATE TABLE repos(id TEXT PRIMARY KEY, origin_url TEXT);
CREATE TABLE sessions(id TEXT PRIMARY KEY,
                      repo_id TEXT, machine TEXT, agent TEXT,
                      agent_session_id TEXT, name TEXT, branch TEXT, pr INTEGER,
                      base_commit TEXT, cwd TEXT, created_at INTEGER, archived_at INTEGER);
CREATE TABLE snapshots(id INTEGER PRIMARY KEY, session_id TEXT, captured_at INTEGER,
                       head_commit TEXT, reason TEXT);
CREATE TABLE snapshot_files(snapshot_id INTEGER, role TEXT,
                            original_path TEXT, blob TEXT, bytes INTEGER);
CREATE TABLE session_commits(session_id TEXT, commit_hash TEXT, patch_id TEXT, first_snapshot INTEGER,
                             PRIMARY KEY (session_id, commit_hash));
