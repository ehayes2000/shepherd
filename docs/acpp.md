# ACP++: shepherd's agent event protocol

ACP++ is an append-only event log. Each entry is an **envelope** wrapping one **JSON-RPC 2.0 frame**. The frames
are [Agent Client Protocol](https://agentclientprotocol.com) (Zed) messages wherever ACP has a word for the
thing, and namespaced extension methods where it doesn't. ACP itself reserves `_`-prefixed methods and `_meta`
fields for extensions, so ACP++ is a strict superset that any ACP tooling can partly read.

Everything shepherd knows about an agent is a fold over its log: status, turns, tool calls, attention, name,
and worktree. This gives us:
- **One model for every source.** A real ACP connection, Claude Code hooks, Codex hooks, and shepherd itself all
  produce the same kind of frames.
- **Replay.** Restart the host and fold the log to recover state. To reproduce a bug, attach the log.
- **Extensibility.** Adding a new event type means adding a method name. Old readers skip it, and nothing migrates.
- **Future features come almost free.** Loops and triggers become subscriptions to frames. An orchestrator is an
  agent that reads logs and appends frames. Multi-machine means streaming logs between peers.

## Envelope

```json
{
  "acpp": 1,
  "seq": 1042,
  "id": "01JBX7Q5W2M9K3...",
  "ts": "2026-10-29T12:00:00.123Z",
  "machine": "m_7f3a9c",
  "session": "s_01jbx7p...",
  "source": { "kind": "hook", "agent": "claude-code" },
  "dir": "from_agent",
  "frame": { "jsonrpc": "2.0", "method": "session/update", "params": { ... } }
}
```

| field     | meaning |
|-----------|---------|
| `acpp`    | Envelope version. It only bumps for breaking *envelope* changes, never for new frame types. |
| `seq`     | Monotonic per log, starting at 1, gapless. Assigned by the host at append time. Readers resume with `from: seq`, the last seq they already have, and receive the entries after it. |
| `id`      | ULID, globally unique. Used to dedupe when logs are relayed between machines. |
| `ts`      | Host wall-clock time at append. Writers emit RFC 3339 UTC with ms; readers accept any ISO 8601 instant jiff parses and keep the original text. Ordering within a log comes from `seq`; across logs, by instant. |
| `machine` | ID of the machine whose host appended the entry: its iroh node id, lowercase hex (T29). Logs from before T29 say `local`, which reads as the machine whose log it is. |
| `session` | Shepherd session ID (`[a-z0-9_-]{1,64}`, lowercase only so case-insensitive filesystems can't alias; the runtime uses `s_<lowercase ulid>`), or `null` for host-level entries. |
| `source`  | Who produced the frame. `kind` ∈ `acp` (a real ACP wire), `hook` (translated from agent hooks), `shepherd` (the host itself), `ext` (third-party producer). Optional `agent` names the agent. |
| `dir`     | `to_agent`, `from_agent`, or `internal`. Gives ACP direction for wire frames. |
| `frame`   | Exactly one JSON-RPC 2.0 message: request, response, or notification. |

Unknown envelope fields must be preserved and ignored.

## Frames

### Standard ACP frames
These are used verbatim with their ACP meaning: `session/prompt` (request and response with `stopReason`),
`session/update` (message, thought, and user chunks, `tool_call`, `tool_call_update`, `plan`, mode and
command updates), `session/request_permission`, `session/cancel`, `elicitation/*`, and so on. Shepherd keeps its own typed subset of ACP
in `core::acpp::acp`, pinned to **ACP protocol v1 / schema 1.13.0** (see decision 0006). Every enum in it is open,
with a catch-all variant. Upgrading the pin is a PM decision.

**Synthesized frames.** If the source isn't a real ACP wire (`source.kind == "hook"`), the producer may
*synthesize* ACP frames so the fold logic is shared:
- a user prompt → a `session/prompt` request with a producer-assigned `id`
- the turn ending → the matching `session/prompt` response with `stopReason`
- tool use → `session/update` `tool_call` / `tool_call_update`, using the agent's tool-use ID as `toolCallId`
- a permission prompt → a `session/request_permission` request. Its outcome is inferred from later frames,
  because the user answers in the agent's own TUI.

A synthesized frame must carry `"_meta": { "shepherd": { "synthesized": true } }` in its params. Consumers
can then tell a real ACP exchange from a reconstruction.

### Extension frames
These are for facts ACP has no message for. The method name is `_<namespace>/<area>/<event>`:

- `_shepherd/...` is reserved for shepherd itself. The initial set:

| method | params (sketch) |
|---|---|
| `_shepherd/session/created`   | `{ name, workspace, worktree, branch, agent, machine, profile?, cwd? }` (profile and cwd: what resume runs again; a `branch` with no `worktree` and no `cwd`: its worktree is being made, and `worktree/created` gives the cwd; a `branch` with a `worktree`: it started in that existing managed worktree, which is its own from then on) |
| `_shepherd/session/renamed`   | `{ name, auto: bool }` |
| `_shepherd/session/closed`    | `{ worktree_removed: bool, worktree_kept? }` (`worktree_kept`: why closing kept a worktree it tried to remove) |
| `_shepherd/session/failed`    | `{ reason }` (it never got a PTY, e.g. its worktree couldn't be made) |
| `_shepherd/session/pr`        | `{ pr? }` with `pr` = `{ number, state, checks }` (the branch's pull request changed; absent: none, or none can be found out. `state`: `open`, `closed`, `merged`; `checks`: `none`, `pending`, `passing`, `failing`; unknown values are kept) |
| `_shepherd/agent/started`     | `{ agent, agent_session_id?, pid?, command, transcript_path? }` (source `shepherd`: a resume respawned the agent; `transcript_path`: the hook payload's, where the agent writes that agent session's transcript) |
| `_shepherd/agent/idle`        | `{ inferred? }` (agent is at its prompt with nothing to do; `inferred`: no agent signal said so and the host inferred it, `"quiet"` or `"quiet-after-permission"`) |
| `_shepherd/agent/exited`      | `{ code?, signal? }` |
| `_shepherd/worktree/created`  | `{ path, branch, base, fetch_error?, post_create? }` (`path` is the session's cwd from then on; `fetch_error`: `base` is as last fetched, or local; `post_create`: the hooks run there before the PTY spawns, each then a `worktree/hook`) |
| `_shepherd/worktree/hook`     | `{ hook, exit_code?, output_tail, error?, path? }` (`output_tail`: the last 4 KiB, control characters stripped; `error`: why it didn't exit normally or couldn't run, e.g. a timeout; `path`: the worktree it ran in, set on an orphan's pre-remove hooks in the host log) |
| `_shepherd/worktree/removing` | `{ path, force?, pre_remove? }` (closing is removing the session's worktree: `force` even if that loses work; `pre_remove`: the hooks run there first, each then a `worktree/hook`; `session/closed` ends it. In the host log: the cleanup view is removing an orphan worktree, its hooks' `worktree/hook`s in the host log too, and `worktree/removed` ends it) |
| `_shepherd/worktree/removed`  | `{ path, removed: bool, kept? }` (host log: how removing the orphan worktree at `path` went; neither `removed` nor `kept`: it was already gone) |
| `_shepherd/workspace/selected`| `{ repo }` (host log) |
| `_shepherd/machine/renamed`  | `{ name }` (host log: the host started under a name other than the last one recorded, its first start included; see `core::machine`) |
| `_shepherd/peer/trusted`     | `{ node, name }` (host log: this machine trusts machine `node`, an iroh node id like `machine`, which calls itself `name` (display-safe); recorded by both sides of a pairing. A later one for the same node renames it. See `core::host::peering`) |
| `_shepherd/peer/removed`     | `{ node }` (host log: `shepherd unpair`; this machine no longer trusts `node` until they pair again) |

The trusted set is the fold of these two over the host log, and is all the host's network endpoint lets in. Pairing
itself is not logged, only its result: the client↔host requests `_shepherd/pair/start` (a single-use ticket, expiring
after 10 minutes), `_shepherd/pair/wait`, `_shepherd/pair/join { ticket }`, `_shepherd/peer/list` and
`_shepherd/peer/remove { peer }` are in `core::wire`. So is `_shepherd/tunnel { machine }` (T31a): as the first request on
a client's conn to its own host, it makes the conn a tunnel to a trusted machine's host, which the client then speaks to
directly (the tunnel itself is not logged either).

- `_<vendor>/...` is for anyone else, e.g. `_claude-code/...` for hook data with no ACP or shepherd equivalent.
  When a vendor frame turns out to be generally useful, promote it to a `_shepherd/` method. Never repurpose
  an existing method.

Extension frames are usually notifications. Use a request/response pair only for an actual request/response.

## Rules for producers and consumers

1. **Append-only.** Never rewrite or delete an entry. To correct something, append another frame.
2. **Lossless.** Readers keep each envelope's raw JSON next to the typed parse. An unknown method parses as
   `Frame::Unknown(raw)` and folds as a no-op. It is never an error and never dropped.
3. **Ignore what you don't understand.** That covers unknown methods, fields, `_meta` keys, and `stopReason`
   values. A fold must never panic on any well-formed JSON-RPC frame.
4. **Additive evolution.** Adding a field or method needs no version bump. Changing a method's meaning needs a new
   method name (`.../v2`). Old logs are replayed forever, so fold code keeps supporting old methods.
5. **`_meta` keys are namespaced** (`shepherd.*`, `<vendor>.*`).
6. **Single writer per log.** Only the host appends. Other producers send the host what they have: `shepherd hook` sends
   the agent's raw hook payload (`_shepherd/hook/frame { session, agent, payload }`), which the host translates into frames.
   The host assigns `seq`, `id`, `ts`, and `machine`.
7. **Log what changes state. Don't log streams.** Raw PTY bytes are not logged. They flow over the live
   connection only.

## State is a fold

```
host_state    = logs.fold(HostState::default(), apply)
session_state = session_log.fold(SessionState::default(), apply)
```

Host state changes **only** by appending a frame and applying it. Core never mutates state any other way.
This is enforced by test: in the scenario harness, folding the persisted logs from scratch must equal the live state.

## Storage (local adapter)

```
~/.shepherd/
  host.jsonl                     # session == null entries
  sessions/<session-id>.jsonl    # one log per session
```

JSONL, one envelope per line, one exclusive writer per root (lock file). An append survives a process crash, not power
loss (decision 0009). On open, bytes after the last newline, or a trailing run of only NUL bytes and newlines, are
truncated and reported. Any complete line that doesn't parse is `Corrupt`. Snapshots and compaction are deferred until replay is measurably slow.
