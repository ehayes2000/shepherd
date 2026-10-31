# Codex hook fixtures

Hand-written hook payloads (stdin JSON) for `tests/codex_hooks.rs`, `tests/subagents.rs` and the `codex` scenario.
They were not recorded from a real `codex` run: builders never run `codex`. Each follows the documented schema:

- The Codex hooks reference: <https://developers.openai.com/codex/hooks> (redirects to
  <https://learn.chatgpt.com/docs/hooks>), read 2026-10-30; its `SubagentStart`/`SubagentStop` sections re-read
  2026-10-31.
- The command-input structs in `codex-rs/hooks/src/schema.rs` (`PreToolUseCommandInput`, `StopCommandInput`,
  `SubagentStartCommandInput`, `SubagentStopCommandInput`, ...), openai/codex at rust-v0.166.0, and for subagents the
  files named below, at the same tag.

## Documented vs inferred

Documented means the field name, type and meaning are in the reference or the schema structs, and its value is one
they list. Inferred means a value we picked that fits the documented type.

| fixture | documented | inferred |
|---|---|---|
| all | common fields `session_id`, `transcript_path` (string or null), `cwd`, `hook_event_name`, `model`; `permission_mode` on the events that carry it; `turn_id` on turn-scoped events | the id, path, model and `turn_id` values |
| `SessionStart.{startup,compact,clear}` | `source` (`startup`, `resume`, `clear`, `compact`, `fork`) | the new session id after `/clear`, and its own rollout file named by that thread id (`rollout_file_name.rs`) |
| `UserPromptSubmit` | `prompt` | prompt text |
| `PreToolUse.Bash`, `PostToolUse.Bash` | `tool_name` (`Bash`), `tool_input.command`, `tool_use_id`, `tool_response` | `call_bash_1`, the output text |
| `PermissionRequest.Bash` | `tool_name`, `tool_input` including `tool_input.description` (may be null), and no `tool_use_id` | that its `tool_input` otherwise equals the matching `PreToolUse`'s |
| `PreToolUse.apply_patch` | `tool_name` (`apply_patch`), `tool_input.command` holding the patch | the patch text and its `*** Update File:` line |
| `PreToolUse.mcp` | MCP tool names like `mcp__fs__read`; MCP tools send all their arguments as `tool_input` | the arguments |
| `PreToolUse.Bash.subagent` | `agent_id`, `agent_type` on hooks fired inside a subagent (schema: optional on tool and prompt inputs) | `agent-7f3a`, `explorer` |
| `Stop`, `Stop.continued` | `stop_hook_active`, `last_assistant_message` (string or null) | message text |
| `Interrupt` | runs when the user interrupts an active main-thread turn; not for idle threads or subagents | |
| `PreCompact`, `PostCompact` | `trigger` (`manual`, `auto`); no summary field | |
| `SessionEnd` | `reason` (currently always `other`); no `model` or `permission_mode` | |
| `SubagentStart` | `turn_id` (the subagent's), `agent_id`, `agent_type` (`SubagentStartCommandInput`); the parent's `session_id` ("Subagent hooks use the parent session id") | `explorer` |
| `SubagentStop` | `turn_id`, `agent_transcript_path` (string or null), `stop_hook_active`, `agent_id`, `agent_type`, `last_assistant_message` (`SubagentStopCommandInput`) | the message |
| `PreToolUse.spawn_agent`, `PostToolUse.spawn_agent` | `spawn_agent` is a local function tool, named `spawn_agent` in hook stdin (`HookToolName::spawn_agent` in `codex-rs/core/src/tools/hook_names.rs`, matcher alias `Agent`); its input is its arguments, `message` and optional `agent_type` (`SpawnAgentArgs` in `codex-rs/core/src/tools/handlers/multi_agents/spawn.rs`) | that its `PreToolUse` fires (the reference's "other local function tools" rows; "some specialized tool paths can opt out"); the `tool_response` text |
| `PermissionRequest.Bash.subagent`, `PostToolUse.Bash.subagent` | `agent_id`, `agent_type` optional on `PermissionRequestCommandInput` and `PostToolUseCommandInput` | that a subagent's approval fires `PermissionRequest` (the reference doesn't say) |
| `PreModelRequest` | an event in `HookEventName` (stands in for any hook without a mapping) | its fields beyond the common ones |

## Facts from the reference the translator relies on

- `Interrupt` fires when the user interrupts an active turn on the main thread. The reference doesn't say whether
  `Stop` fires too; a `Stop` after `Interrupt` finds no open prompt and only goes idle.
- `PermissionRequest` "runs when Codex is about to ask for approval", and not for commands that need none.
- `SessionStart` fires with `source: compact` after compaction.
- `Stop` can be blocked by a hook, and Codex then keeps working (`stop_hook_active: true` on the next `Stop`).
- `SessionEnd` runs when an open conversation is archived or deleted, or after 30 minutes idle with no client open.
- Codex has no `Notification`, `PostToolUseFailure`, `PermissionDenied` or `StopFailure` hook.
- A thread-spawned subagent fires `SubagentStart` and `SubagentStop` in place of `SessionStart` and `Stop`
  (`StartHookTarget::SubagentStart` in `codex-rs/hooks/src/events/session_start.rs`, `StopHookTarget::SubagentStop`
  in `stop.rs`). `SessionEnd` and `Interrupt` don't run for subagents. Plain stdout from a `SubagentStart` hook
  becomes model context, so `shepherd hook` must print nothing (it never does).
- `spawn_agent` without an `agent_type` spawns the `default` role (`DEFAULT_ROLE_NAME` in
  `codex-rs/core/src/agent/role.rs`). That `SubagentStart`'s `agent_type` is then `default` is inferred.
- Codex has no hook for a subagent asking the user a question, so Codex subagents only block on permission.
