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
  "session": "s_01JBX7P...",
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
| `ts`      | Host wall-clock time at append (RFC 3339, UTC, ms). Informational. Ordering comes from `seq`. |
| `machine` | ID of the machine whose host appended the entry. |
| `session` | Shepherd session ID, or `null` for host-level entries (workspace and machine events). |
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
| `_shepherd/session/created`   | `{ name, workspace, worktree, branch, agent, machine }` |
| `_shepherd/session/renamed`   | `{ name, auto: bool }` |
| `_shepherd/session/closed`    | `{ worktree_removed: bool }` |
| `_shepherd/agent/started`     | `{ agent, agent_session_id?, pid?, command }` |
| `_shepherd/agent/idle`        | `{}` (agent is at its prompt with nothing to do) |
| `_shepherd/agent/exited`      | `{ code?, signal? }` |
| `_shepherd/worktree/created`  | `{ path, branch, base }` |
| `_shepherd/worktree/hook`     | `{ hook, exit_code, output_tail }` |
| `_shepherd/workspace/selected`| `{ repo }` (host log) |

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
6. **Single writer per log.** Only the host appends. Other producers, like `shepherd hook`, send frames to the
   host, which assigns `seq`, `id`, `ts`, and `machine`.
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

JSONL, one envelope per line. Append, then flush. On open, a torn final line is truncated with a warning.
Corruption anywhere else is an error. Snapshots and compaction are deferred until replay is measurably slow.
