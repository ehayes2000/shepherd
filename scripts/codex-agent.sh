#!/usr/bin/env bash
# Runs a shepherd builder or reviewer as a Codex agent (`codex exec`), in place of a Claude Code subagent.
#
#   scripts/codex-agent.sh build  <task-id> <branch> <brief-file>   # new worktree on <branch> from main, builder role
#   scripts/codex-agent.sh review <task-id> <branch> <brief-file>   # detached worktree of <branch>, reviewer role
#   scripts/codex-agent.sh resume <task-id> <build|review> <message-file>   # continue that agent's session
#
# The brief is the task spec plus the PM's notes (what a Claude subagent got as its prompt). The role's standing
# instructions come from .claude/agents/<role>.md, which the agent is told to read. Each run writes, under
# .claude/codex/<task-id>/: <role>.session (Codex session id), <role>.jsonl (events), <role>.err, and <role>.md (the
# agent's final message: its report). The PM runs this in the background and reads <role>.md when it exits.
#
# Model: Codex Sol on the Fast tier (gpt-6.1-sol, service tier "priority"), high reasoning. The user's Codex login is
# used; their ~/.codex config is not (--ignore-user-config). No sandbox, by the user's choice (2026-11-01): the gate needs
# PTYs, local sockets, real host processes, git writes and cargo downloads. The written rules in builder.md and
# reviewer.md still bind the agent.
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd -P)
model=${SHEPHERD_CODEX_MODEL:-gpt-6.1-sol}
flags=(--ignore-user-config -m "$model" -c 'service_tier="priority"' -c 'model_reasoning_effort="high"'
  --dangerously-bypass-approvals-and-sandbox --json)

usage() { sed -n '4,6p' "$0" | sed 's/^# //' >&2; exit 2; }
[ $# -eq 4 ] || usage
cmd=$1 id=$2

case $id in *[!A-Za-z0-9._-]* | '') echo "codex-agent: bad task id: $id" >&2; exit 2 ;; esac
state="$repo/.claude/codex/$id"
mkdir -p "$state"

preamble() { # role worktree branch
  cat <<EOF
You are the shepherd $1 for task $id, working as a Codex agent for the PM (project manager) of this project.
Your working directory is the git worktree $2 (branch $3). The main checkout is $repo: never work there.

Before anything else, read and follow these, in order: CLAUDE.md, .claude/agents/$1.md (your role and its rules), and
docs/architecture.md. Where those files mention skills, read the skill's instructions at .claude/skills/<name>/SKILL.md
(for a builder: tdd, ship, fix-bug, new-port). Where they mention the Agent tool, SendMessage or subagents, those don't
apply to you: you do the work yourself and never start other agents. Never git push, merge into main, or change the main
checkout. Your final message is your report to the PM, in the form your role file asks for.

The PM's brief follows.

EOF
}

run() { # role cwd prompt-file
  local role=$1 cwd=$2 prompt=$3
  (cd "$cwd" && codex exec "${flags[@]}" -C "$cwd" -o "$state/$role.md" - <"$prompt") \
    >"$state/$role.jsonl" 2>"$state/$role.err" || true
  python3 -I - "$state/$role.jsonl" "$state/$role.session" <<'EOF'
import json, sys
for line in open(sys.argv[1]):
    try:
        event = json.loads(line)
    except ValueError:
        continue
    tid = event.get("thread_id") or event.get("session_id")
    if event.get("type") in ("thread.started", "session.started") and tid:
        open(sys.argv[2], "w").write(tid + "\n")
        break
EOF
  report "$role"
}

report() { # role
  echo "codex-agent: $cmd $id ($1) finished; report: $state/$1.md"
  [ -s "$state/$1.md" ] || { echo "codex-agent: no report; last errors:" >&2; tail -20 "$state/$1.err" >&2; exit 1; }
}

case $cmd in
  build)
    branch=$3 brief=$4
    wt="$repo/.claude/worktrees/codex-$id"
    if [ ! -d "$wt" ]; then
      if git -C "$repo" show-ref --verify --quiet "refs/heads/$branch"; then
        git -C "$repo" worktree add -q "$wt" "$branch"
      else
        git -C "$repo" worktree add -q -b "$branch" "$wt" main
      fi
    fi
    { preamble builder "$wt" "$branch"; cat "$brief"; } >"$state/build.prompt"
    run build "$wt" "$state/build.prompt"
    ;;
  review)
    branch=$3 brief=$4
    wt="$repo/.claude/worktrees/codex-review-$id"
    [ -d "$wt" ] && git -C "$repo" worktree remove --force "$wt"
    git -C "$repo" worktree add -q --detach "$wt" "$branch"
    { preamble reviewer "$wt" "$branch (detached; read-only: change no files)"; cat "$brief"; } >"$state/review.prompt"
    run review "$wt" "$state/review.prompt"
    git -C "$repo" worktree remove --force "$wt"
    ;;
  resume)
    role=$3 message=$4
    case $role in build | review) ;; *) usage ;; esac
    [ -s "$state/$role.session" ] || { echo "codex-agent: no $role session recorded for $id" >&2; exit 1; }
    session=$(cat "$state/$role.session")
    case $role in build) wt="$repo/.claude/worktrees/codex-$id" ;; review) wt="$repo/.claude/worktrees/codex-review-$id" ;; esac
    if [ "$role" = review ] && [ ! -d "$wt" ]; then
      echo "codex-agent: the review worktree is gone; start a new review instead" >&2; exit 1
    fi
    (cd "$wt" && codex exec resume "${flags[@]}" -o "$state/$role.md" "$session" - <"$message") \
      >>"$state/$role.jsonl" 2>>"$state/$role.err" || true
    report "$role"
    ;;
  *) usage ;;
esac
