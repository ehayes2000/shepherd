-- A version 1 archive index, written by hand from the proposal's sketch (docs/proposals/conversation-archive.md), not
-- by the crate's own migration code, so the migration test proves a real v1 file moves forward.
-- Times are microseconds since the Unix epoch. The blobs are the sha256 of "fixture transcript\n" and
-- "fixture subagent\n"; the test stores those contents.
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

INSERT INTO repos VALUES ('1111111111111111111111111111111111111111', 'git@example.com:me/project.git');
INSERT INTO sessions VALUES ('s-v1', '1111111111111111111111111111111111111111', 'laptop', 'claude',
                             '0b6f3c1e-7d2a-4c55-9f00-000000000001', 'fix the parser', 'fix-parser', 7,
                             '2222222222222222222222222222222222222222', '/work/project/.shepherd/fix-parser',
                             1767225600000000, NULL);
INSERT INTO snapshots VALUES (1, 's-v1', 1767229200123456, '3333333333333333333333333333333333333333', 'turn_end');
INSERT INTO snapshot_files VALUES (1, 'main', '/home/me/.claude/projects/p/0b6f3c1e.jsonl',
                                   'a4efb28c1727da06107bee9c9eaedbe5dd7815d34263b8659b1692246b50a9ae', 19);
INSERT INTO snapshot_files VALUES (1, 'subagent', '/home/me/.claude/projects/p/0b6f3c1e/subagents/a.jsonl',
                                   '4aa8faba1f4e0dcb5024d987268590fcb59cad7a95474d8a144e16047e81cc15', 17);
INSERT INTO session_commits VALUES ('s-v1', '3333333333333333333333333333333333333333',
                                    '4444444444444444444444444444444444444444', 1);
PRAGMA user_version = 1;
