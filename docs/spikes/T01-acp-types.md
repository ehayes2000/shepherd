# T01 spike: ACP schema types in core

Date: 2026-10-29. Everything below was checked against the published crate source and a scratch crate outside
the repo. Nothing here is from memory.

## TL;DR

- The ACP types live in **`agent-client-protocol-schema`**. The latest release is **1.13.0** (2026-10-27), and it
  speaks ACP **protocol version 1** (`ProtocolVersion::LATEST == V1`). v2 is a draft behind the
  `unstable_protocol_v2` feature.
- It is pure: no async runtime, no IO, and none of `std::fs/env/process/net/thread/time`. With
  `default-features = false` it **could** join `CORE_ALLOWED`.
- **Recommendation: (b), vendor a minimal typed subset in `core::acpp::acp`.** The schema types are not
  lossless. They drop unknown fields, and they **fail to parse** unknown `sessionUpdate`, `stopReason`, content
  block and permission-option values. That breaks acpp.md rules 2 and 3 exactly where the fold needs them. They
  would also take core from 13 external crates to 35, and they change too fast to pin cheaply.
- ACP **does** have a stable compaction update: `session/update` with `sessionUpdate: "compaction_update"`,
  stabilized in schema 1.11.0. T15 can map `PreCompact` to it.

## 1. Which crate, which version

| crate | latest | role |
|---|---|---|
| `agent-client-protocol-schema` | 1.13.0 (2026-10-27), MSRV 1.88, edition 2024, Apache-2.0 | wire types only |
| `agent-client-protocol` | 3.6.0 (2026-10-28) | the SDK: connection, `Agent`/`Client` traits. Depends on the schema crate at **`=1.13.0`** |

Release cadence: the schema crate shipped 8 versions between 2026-09-18 and 2026-10-27 (1.9.0 to 1.13.0). The SDK
pins the schema crate exactly, so whichever schema version core pins also fixes the SDK version the adapters can
use later (for ACP-hosted sessions).

## 2. Dependency tree (real `cargo tree`)

Scratch crate: `cargo add agent-client-protocol-schema@=1.13.0 --no-default-features`, then
`cargo tree -e normal`. Toolchain rustc 1.96.0.

```
agent-client-protocol-schema v1.13.0
├── anyhow v1.0.104
├── derive_more v2.1.1            (features: from, display)
│   └── derive_more-impl (proc-macro) → convert_case, unicode-segmentation, proc-macro2, quote, syn 2, unicode-xid
├── serde v1.0.229                (features: derive, rc)
│   └── serde_core, serde_derive (proc-macro) → syn 3
├── serde_json v1.0.154           (features: preserve_order, raw_value)
│   └── indexmap → equivalent, hashbrown; foldhash, itoa, memchr, zmij, serde_core
├── serde_with v3.24.0            (features: json)
│   └── serde_with_macros (proc-macro) → darling 0.24 → darling_core, darling_macro, ident_case, strsim, syn 3
└── strum v0.28.0
    └── strum_macros (proc-macro) → heck, syn 2
```

- No defaults: **33** unique crates (counting the schema crate itself). Core today has **13** external crates.
  Core plus the schema crate would have **35**, so it **adds 22**: `agent-client-protocol-schema anyhow
  convert_case darling darling_core darling_macro derive_more derive_more-impl equivalent foldhash hashbrown heck
  ident_case indexmap serde_with serde_with_macros strsim strum strum_macros unicode-segmentation unicode-xid`,
  plus a second `syn` major (v2, next to core's v3).
- Default features add `schemars` (+ `schemars_derive`, `dyn-clone`, `ref-cast`, `serde_derive_internals`), for
  **39** crates. Always use `default-features = false`.
- For contrast, the SDK `agent-client-protocol` 3.6.0 pulls in 62 crates, including `futures`,
  `futures-concurrency`, `tracing`, `uuid` and `rustc-hash`. It must never be a core dependency.
- Feature unification: depending on it turns on `serde_json/preserve_order` and `raw_value` for the **whole
  workspace**. That is harmless, and arguably good for log readability, but it is a side effect.
- `grep -rnE "std::(fs|env|process|net|thread|time)|Instant|SystemTime"` over its `src/` finds nothing.

**Can it join `CORE_ALLOWED`?** By the spec's test (only serde-level deps, no async runtime), yes, as long as
`default-features = false`. `anyhow`, `derive_more`, `strum` and `serde_with` are all pure. Note that
`check-arch.sh` only checks core's **direct** deps, so allowing it means adding one name and implicitly letting
in 21 more transitive crates that the gate never sees.

## 3. Lossless behavior (empirical)

Probe: parse a JSON value into the schema type, serialize it back, compare `serde_json::Value` equality
(the T03 acceptance criterion). Source: `tests/lossless.rs` in the scratch crate.

| input | result |
|---|---|
| known `tool_call`, `prompt`, `request_permission`, `cancel`, `compaction_update` | lossless |
| `_meta: { shepherd: { synthesized: true } }` | lossless (`Meta` is a free-form map) |
| unknown field inside `update` (`futureField`) | **LOSSY**: field dropped |
| unknown field in `params` (`futureTop`) | **LOSSY**: field dropped |
| unknown `sessionUpdate` (`future_thing`) | **PARSE ERROR** (`SessionUpdate` has no catch-all) |
| unknown `stopReason` (`rate_limited`) | **PARSE ERROR** (`StopReason` has no `Other`) |
| unknown content block `type` | **PARSE ERROR** |
| unknown permission option `kind` | **PARSE ERROR** |
| unknown tool `kind` (`teleport`) | **LOSSY**: `#[serde(other)]` turns it into `other`, which then serializes as absent |
| unknown tool `status` (`paused`) | **LOSSY**: `DefaultOnError` turns it into `None`, dropped |
| `_meta: "oops"` (wrong type) | **LOSSY**: `DefaultOnError`, dropped |
| unknown compaction `status` (`_claude/paused`) | lossless (`Other(String)`, untagged) |

Source facts behind this: no struct uses `deny_unknown_fields`, and none collects extra fields with
`#[serde(flatten)] extra: Map` (the 17 `flatten` uses are structural). 318 fields use `DefaultOnError`, which
silently replaces malformed values. Only 11 enums have an `Other(String)` escape hatch. The enums are
`#[non_exhaustive]`, which only affects Rust matching and does nothing for deserialization.

What this means for us: acpp.md already requires keeping the raw `Value` next to the typed parse, so data loss
on re-serialization is avoidable either way: we would always write the raw value. The real problem is the
**parse errors**. A `session/prompt` response with a new `stopReason` would fail the typed parse, become
`Unknown`, and the fold would never see the turn end, so the session would stay `Working` forever. acpp.md rule 3
says unknown `stopReason` values must be ignored, not make the frame invisible. The same happens to a
`tool_call` whose content block type is new. Working around that on top of the schema types (parse raw first,
then patch) costs more than writing the six structs we need.

## 4. Recommendation: (b) vendor a minimal typed subset

Put a hand-written subset in `core::acpp::acp`, with these rules:

1. Type only the methods, variants and fields the folds read (section 5). Everything else stays raw `Value`.
2. Every enum we type gets a catch-all: `StopReason::Other(String)`, `ToolCallStatus::Other(String)`,
   `ToolKind::Other(String)`, `SessionUpdate::Other { kind: String, raw: Value }`,
   `ContentBlock::Other(Value)`. Unknown values parse, and the fold treats them sensibly. For example, any
   prompt response ends the turn, whatever its `stopReason`.
3. Never re-serialize from the typed struct for storage. Store the raw frame, and use the typed view only for
   reading. Producers (T15) build frames with `serde_json::json!` or with small `Serialize` structs, and
   round-trip tests prove the output parses.
4. Field names and wire spellings come from schema 1.13.0 / protocol v1. Cite the version in a module doc
   comment.
5. Optional, and cheap: add `agent-client-protocol-schema = { version = "=1.13.0", default-features = false }` as a
   core **dev-dependency** for one conformance test. Every fixture frame we produce must also parse with the
   official types. That catches spelling drift without putting the crate in the production graph.
   `CORE_ALLOWED` only checks normal deps, so this needs no allowlist change. The PM decides whether that's
   worth it.

Why not (a):
- It fails the lossless and ignore-unknown rules where it matters (section 3).
- It takes core from 13 external crates to 35, with 4 new proc-macro crates, and none of that is policed by the gate.
- It churns: 8 releases in 6 weeks, and the exact `=` pin couples core to the SDK version the adapters use later.
- We read about 15 fields across 5 methods. The vendored code is roughly 150 lines.

If the PM still prefers (a), the minimum is `default-features = false`, an exact `=1.13.0` pin, a decision-log
entry, and a T03 design that parses `Value` first and only tries the typed parse per method, falling back to
`Unknown` without losing the frame. That fallback still leaves the stopReason/permission-kind problem above
unsolved.

## 5. What M1–M2 needs from ACP

Pinned reference: **ACP protocol version 1**, schema crate **1.13.0**, docs at
<https://agentclientprotocol.com/protocol/v1>.

### Methods

| method | kind | used by | fields the fold reads |
|---|---|---|---|
| `session/prompt` | request | T05 (→ Working, turn count), T15 (synthesized from `UserPromptSubmit`), T24 (last prompt text) | `id`, `params.sessionId`, `params.prompt[]` (only `type: "text"` → `text`; other blocks raw), `params._meta` |
| `session/prompt` | response | T05 (→ Idle, clears NeedsPermission), T15 (from `Stop`) | `id`, `result.stopReason` (open enum: `end_turn`, `max_tokens`, `max_turn_requests`, `refusal`, `cancelled`, `Other`), or `error` |
| `session/update` | notification | T05, T15 | `params.sessionId`, `params.update.sessionUpdate`, `params._meta` (variants below) |
| `session/request_permission` | request | T05 (→ NeedsPermission { title }), T15 | `id`, `params.toolCall.toolCallId`, `params.toolCall.title`, `params.options[]` (raw is fine; hooks don't give us an outcome) |
| `session/cancel` | notification | listed in acpp.md; no M1–M2 fold reads it | `params.sessionId`. Trivial to type; the fold can treat it as a no-op for now |

The `session/request_permission` **response** (`outcome: cancelled | selected{optionId}`) never appears in M2,
because the hook path infers outcomes from later frames. Keep it raw.

Design note for T03: a JSON-RPC response has no `method`. To recognize a `session/prompt` response, the fold has to
remember the method of each pending request `id`. So `Frame` should parse responses as a generic
`Response { id, result | error }` (raw result), and the session fold should keep `pending: id → method` to
interpret them. Don't try to type responses at parse time.

### `session/update` variants

| `sessionUpdate` | needed | used by | fields |
|---|---|---|---|
| `user_message_chunk` | yes | T15 synthesizes it with the prompt; T24 can read text | `content` (ContentBlock: `text` typed, others raw), `messageId?` |
| `tool_call` | yes | T05 (→ Working { tool }), T15 (`PreToolUse`) | `toolCallId`, `title`, `kind?` (open enum, default `other`), `status?` (default `pending`; open enum), `name?`, `rawInput?` (raw) |
| `tool_call_update` | yes | T05 (completed/failed clears the tool, clears NeedsPermission), T15 (`PostToolUse`) | `toolCallId`, `status?`, `title?`, `kind?`; everything else raw |
| `compaction_update` | yes (M2, T15 `PreCompact`) | T15; T05 may ignore it | `compactionId`, `status` (`in_progress`, `completed`, `failed`, `cancelled`, `Other`), `summary?`, `error?` (raw) |
| `agent_message_chunk`, `agent_thought_chunk` | no (hooks don't produce them) | ACP-hosted sessions later | keep as `Other` |
| `plan`, `available_commands_update`, `current_mode_update`, `config_option_update`, `session_info_update`, `usage_update`, `notice`, `compaction_summary_chunk` | no | | keep as `Other` |
| unstable: `plan_update`, `plan_removed`, `subagent_update`, `session_message`, `session_message_chunk` | no | | keep as `Other` |

Tool call status values (v1): `pending`, `in_progress`, `completed`, `failed`. Tool kinds (v1): `read`, `edit`,
`delete`, `move`, `search`, `execute`, `think`, `fetch`, `switch_mode`, `other`.

### Compaction

Yes. `compaction_update` and `compaction_summary_chunk` were stabilized in schema **1.11.0** (2026-10-09,
upstream PR #2334). Wire shape:

```json
{ "sessionUpdate": "compaction_update", "compactionId": "c1", "status": "in_progress",
  "summary": [ContentBlock], "error": "...", "_meta": {} }
```

Semantics: this is an upsert keyed by `compactionId`. The first update fixes the compaction's position on the
timeline, and later updates with the same ID patch it. `summary`, `error` and `_meta` have patch semantics:
omitted means unchanged, `null` means cleared. The spec says agents MUST only send it when the client advertised
`session.compaction`. Synthesized hook frames have no handshake, but our log is the only consumer, so that's fine.
T15 should emit `compaction_update { status: "in_progress" }` on `PreCompact`, with a deterministic `compactionId`
taken from the translation state. If T15 finds a hook that marks the end of compaction, it should emit a second
update with the same ID and `completed`. That belongs to T15, not this spike.

## Follow-ups for the PM (not done here)

- acpp.md says "Pin the ACP schema version in `Cargo.toml`". Under (b) there's no Cargo pin, so reword it to
  "pin the ACP schema version (protocol v1, schema 1.13.0) in `core::acpp::acp`". Upgrades stay a PM decision.
- `check-arch.sh` doesn't police transitive deps of core. That's fine with (b), but worth knowing if any crate
  ever joins `CORE_ALLOWED`.
- T03's acceptance should say that `StopReason`, `ToolCallStatus`, `ToolKind`, `SessionUpdate` and
  `ContentBlock` are open enums, and that responses are typed by correlating them with their request.

## Reproduce

```
cargo new --lib acpspike && cd acpspike
cargo add agent-client-protocol-schema@=1.13.0 --no-default-features
cargo add serde serde_json
cargo tree -e normal
# lossless probe: parse → serialize → compare Value for the inputs in section 3
```
