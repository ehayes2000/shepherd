-- Version 2: the same tables rebuilt STRICT, with NOT NULL, CHECKs on the enums, foreign keys and the lookup indexes.
-- Paths become BLOBs (a Unix path is bytes, not necessarily UTF-8); v1's TEXT paths keep their bytes. A v1 row that
-- breaks a constraint fails the migration, which then rolls back and leaves the file at v1.
-- Runs inside the migration's transaction with foreign keys off; the caller runs `PRAGMA foreign_key_check` after.

CREATE TABLE repos_v2(
    id TEXT PRIMARY KEY NOT NULL,
    origin_url TEXT
) STRICT;
INSERT INTO repos_v2 (id, origin_url) SELECT id, origin_url FROM repos;
DROP TABLE repos;
ALTER TABLE repos_v2 RENAME TO repos;

CREATE TABLE sessions_v2(
    id TEXT PRIMARY KEY NOT NULL,
    repo_id TEXT NOT NULL REFERENCES repos(id),
    machine TEXT NOT NULL,
    agent TEXT NOT NULL CHECK (agent IN ('claude', 'codex')),
    agent_session_id TEXT NOT NULL,
    name TEXT,
    branch TEXT,
    pr INTEGER CHECK (pr > 0),
    base_commit TEXT,
    cwd BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    archived_at INTEGER
) STRICT;
INSERT INTO sessions_v2
    SELECT id, repo_id, machine, agent, agent_session_id, name, branch, pr, base_commit, CAST(cwd AS BLOB),
           created_at, archived_at
    FROM sessions;
DROP TABLE sessions;
ALTER TABLE sessions_v2 RENAME TO sessions;

CREATE TABLE snapshots_v2(
    id INTEGER PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id),
    captured_at INTEGER NOT NULL,
    head_commit TEXT,
    reason TEXT NOT NULL CHECK (reason IN ('turn_end', 'close', 'before_remove', 'manual'))
) STRICT;
INSERT INTO snapshots_v2 SELECT id, session_id, captured_at, head_commit, reason FROM snapshots;
DROP TABLE snapshots;
ALTER TABLE snapshots_v2 RENAME TO snapshots;

CREATE TABLE snapshot_files_v2(
    snapshot_id INTEGER NOT NULL REFERENCES snapshots(id),
    role TEXT NOT NULL CHECK (role IN ('main', 'subagent', 'rollout')),
    original_path BLOB NOT NULL,
    blob TEXT NOT NULL,
    bytes INTEGER NOT NULL CHECK (bytes >= 0),
    PRIMARY KEY (snapshot_id, original_path)
) STRICT;
INSERT INTO snapshot_files_v2
    SELECT snapshot_id, role, CAST(original_path AS BLOB), blob, bytes FROM snapshot_files ORDER BY rowid;
DROP TABLE snapshot_files;
ALTER TABLE snapshot_files_v2 RENAME TO snapshot_files;

CREATE TABLE session_commits_v2(
    session_id TEXT NOT NULL REFERENCES sessions(id),
    commit_hash TEXT NOT NULL,
    patch_id TEXT,
    first_snapshot INTEGER NOT NULL REFERENCES snapshots(id),
    PRIMARY KEY (session_id, commit_hash)
) STRICT;
INSERT INTO session_commits_v2 SELECT session_id, commit_hash, patch_id, first_snapshot FROM session_commits;
DROP TABLE session_commits;
ALTER TABLE session_commits_v2 RENAME TO session_commits;

CREATE INDEX session_commits_by_commit ON session_commits(commit_hash);
CREATE INDEX session_commits_by_patch_id ON session_commits(patch_id);
CREATE INDEX sessions_by_pr ON sessions(repo_id, pr);
CREATE INDEX sessions_by_branch ON sessions(repo_id, branch);
CREATE INDEX sessions_by_agent_session ON sessions(agent, agent_session_id);
CREATE INDEX snapshots_by_session ON snapshots(session_id, captured_at);
CREATE INDEX snapshot_files_by_blob ON snapshot_files(blob);
