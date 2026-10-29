---
name: new-port
description: Add a new port to shepherd (a capability that touches the world - git, pty, fs, processes, agents, network, time). Covers the trait, fake, contract suite, real adapter, and wiring. Use whenever core needs something it can't compute purely.
---

# Adding a port

A port isn't done until all five pieces exist. Build them in this order, because each one is the test for the next.

## 1. Trait and domain types (`crates/core/src/ports/<name>.rs`)
- Name the capability in domain language: `Worktrees`, `AgentProcesses`, `Clock`. Avoid names like `GitCli` or `Pty2`.
- Methods take and return core domain types. No `std::process::Output`, `git2::Oid` or `PathBuf`-as-protocol
  leaking through. If you need a path, wrap it in a domain newtype where it carries meaning (`WorktreePath`).
- Errors are a core `enum` (`thiserror`) of *domain* failures, e.g. `BranchExists` or `RepoNotFound`. Wrap
  unexpected cases as `Other(String)`.
- Keep the trait small. Fewer methods mean an easier fake and a stronger contract.
- If the PM's spec didn't say this trait shape, stop and report your proposed trait before going further.

## 2. Contract suite (`crates/testkit/src/contracts/<name>.rs`)
Copy `contracts/pty.rs`. It has one `pub fn <case>(make: ...)` per case, and a `#[macro_export] <name>_contract!(make)`
that expands to one `#[test]` per case. `make` builds the port from whatever it needs (the Pty port takes an event
sink). **Adding a case means adding it to the macro's list.** There's a comment in the template saying so.

If the port has to be built in several states (missing, failing, holding data, ...), use a fixture trait instead of one
`make` closure. See `contracts/config.rs`: a `ConfigFixture` for the shared cases, and an extra `TextConfigFixture` for
cases only the real adapter can satisfy.
Each case is a plain function that panics on failure. Cover the behavior core relies on, error cases included.
This suite *is* the spec of the port.

Expose it through a macro that expands to **one `#[test]` per case**, so a failure names its case and doesn't hide
the others: `worktrees_contract!(FakeWorktrees::harness)`. Copy the pattern from the `Pty` contract.

Write cases for the ways real resources misbehave, not just the happy path. That means processes that exit
instantly, children that ignore signals, buffers filling up, concurrent use, and partial reads and writes. These are
where adapters break.

## 3. Fake (`crates/testkit/src/fakes/<name>.rs`)
- Stateful and in-memory. It behaves like the real thing, it doesn't just return canned answers.
- It can inject failures, e.g. `fake.fail_next(WorktreeError::BranchExists)`.
- It records calls only if tests need them, and prefers exposing resulting state.
- Its test is one line: `worktrees_contract!(FakeWorktrees::harness);` (expands to one `#[test]` per case).

## 4. Real adapter (`crates/adapters/src/<name>.rs`)
- Thin. Translate the domain call to IO and the IO result back to domain types. No policy.
- Its test runs the **same** suite against real resources in a tempdir:
  `worktrees_contract!(GitWorktrees::temp_harness);`.
- **Run the adapter's contract under parallel load** before you ship, e.g.
  `for i in $(seq 24); do target/debug/deps/<test-bin> & done; wait`, repeated a few times, and report the pass rate.
  Use 24 copies, not more: macOS caps ptys at 511 system-wide, and the Pty contract opens 20 at once, so 48 copies fail for
  reasons unrelated to your change.
  T08's adapter passed 40 out of 40 runs one at a time and failed 4% of runs under load, losing output.
  Run the **whole** adapter test binary, not just your port's tests. Ports interact inside one process (PTY children
  inherit file descriptors and locks across `fork`), and T04's lock bug only showed up next to the PTY tests.
- If a case passes on the fake but fails on the real adapter, fix the fake *and* note it in your report. It means
  core tests were testing a world that doesn't exist.

## 5. Wiring (`crates/shepherd/src/main.rs`, runtime)
Build the real adapter in `main.rs` and pass it into the runtime. Add the fake to the scenario harness defaults.

## Checklist
- [ ] trait + domain errors in core, no IO types leaked
- [ ] contract suite covers happy paths and each error variant core handles
- [ ] fake passes the suite
- [ ] real adapter passes the suite, including under parallel load (pass rate in report)
- [ ] contract macro gives one `#[test]` per case
- [ ] wired in main and the scenario harness
- [ ] `docs/plan.md` ports list updated (tell the PM in your report if you can't edit it)
