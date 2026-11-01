# Shepherd

A terminal agent manager: a sidebar of coding-agent sessions (Claude Code, Codex), each running in a
shepherd-managed git worktree, across multiple machines. `manifesto.md` is the product intent.

Before writing code, read `docs/architecture.md`. Work is tracked per feature in `docs/features/<feature>.md` (a tracker
table plus the task specs); `docs/plan.md` is the generated index (`scripts/tasks.py`). Requests go in `docs/inbox.md`. The PM's operating manual,
including how to take over in-flight work, is `docs/pm.md`.

## How this project is built

This project is built by agents. The main session is the **project manager (PM)**. It owns the plan,
writes task specs, dispatches work, and merges. Feature work is done by builder agents (one task
each, in an isolated worktree) and checked by reviewer agents that see only the diff and the spec. Both run as Codex
agents (Codex Sol, Fast tier) through `scripts/codex-agent.sh`; their role files are `.claude/agents/{builder,reviewer}.md`.

The PM does not write feature code. It may edit docs, the plan, and the gate tooling.

Skills:
- `write-task`: (PM) turn a feature into task specs in its `docs/features/<feature>.md`
- `dispatch`: (PM) run ready tasks through builder → reviewer → merge
- `tdd`: (builder) the red/green/refactor loop and where each kind of test goes
- `new-port`: (builder) add a port: trait, fake, contract suite, real adapter, wiring
- `fix-bug`: (anyone) a failing regression test comes before any fix
- `ship`: (builder) gate, commit, report

## The gate

```
scripts/check.sh      # fmt --check, clippy -D warnings, all tests, architecture rules
```

`scripts/check.sh` must pass before every commit. The pre-commit hook runs it (`git config core.hooksPath .githooks`).
There is no CI; the local gate is the only check. Never use `--no-verify`. If the gate is wrong, fix the gate in its own commit and say why.

## Architecture: the non-negotiables

The full version, with reasons, is in `docs/architecture.md`.

1. **Functional core.** `shepherd-core` is pure, with no IO, threads, clocks, randomness or env.
   All behavior is `App::handle(Event) -> Vec<Command>`.
2. **Ports are traits in core. Adapters implement them.** Anything that touches the world (git, PTYs,
   filesystem, processes, network, time) sits behind a port.
3. **Dependencies point inward.** `core` depends on no internal crate. `tui` and `adapters` depend only on
   `core`. Only the `shepherd` binary crate wires everything together. `scripts/check-arch.sh` enforces this.
4. **Rendering is a pure function of state.** The TUI never mutates domain state. Input is mapped to `Event`s.
5. **Don't reinvent.** Use well-known crates for solved problems (time, encodings, parsing, ...). If the crate
   isn't allowed where you need it, ask the PM to allow it. Never hand-roll it and never compare structured values as strings.
6. If a task seems to need breaking a rule, **stop and escalate to the PM**. Don't work around it. The PM
   either changes the task or records a decision in `docs/architecture.md`.

## Testing rules

1. **TDD.** Write the test, watch it fail *for the right reason*, make it pass, then refactor.
2. **Test behavior, not implementation.** Go through public APIs: `App::handle`, port traits, render output.
   Don't test private helpers. Don't assert on internal struct layout.
3. **Fakes, not mocks.** Every port has a stateful in-memory fake in `shepherd-testkit`. No mocking frameworks.
4. **Contract suites keep fakes honest.** Each port has one shared test suite that runs against both the fake
   and the real adapter. When a fake and the real adapter disagree, it's a bug.
5. **Bug fix = regression test first.** No exceptions (`fix-bug` skill).
6. **Never weaken a test to get green.** Don't delete it, add `#[ignore]`, loosen an assertion or blindly
   accept a snapshot. Changing an existing assertion must be justified in the commit message and the report.
7. **Deterministic.** No sleeps, no wall clock, and no network in core or tui tests. Real-adapter tests use
   tempdirs and clean up after themselves. **No wall-clock performance assertions anywhere in the gate.** To prove
   complexity, count the work (bytes processed, calls made). Timeouts in adapter tests are generous upper bounds on
   liveness, never measurements.
8. **UI is snapshot-tested** with `insta` and ratatui's `TestBackend`. Read every new or changed `.snap.new`
   before accepting it. Never bulk-accept.

## Commits

This is a small personal project and doesn't need extensive testing plans and garbage.
When you make a commit, use a conventional semantic commit message (feat, chore, fix, test, refactor) with a
one-line description of what you did.

If a PR is more than a single commit, the body should contain the commit message of each commit.
Otherwise the body should be left blank.

ALWAYS RUN `scripts/check.sh` BEFORE A COMMIT OR PR (it runs `cargo fmt --check` and `cargo clippy --workspace --all-targets -- -D warnings`).
