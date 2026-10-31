#!/usr/bin/env bash
# Delete scratch paths you created: each argument is one file or directory, named in full (no globs), and it must sit
# inside a temp directory ($TMPDIR, /tmp or /private/tmp). Anything else is refused and nothing is deleted.
# A git worktree is refused too: remove it with `git worktree remove --force <path>`.
#
#   scripts/scratch-rm.sh "$SCRATCH/t41r-target" "$SCRATCH/t41r-probe.log"
set -euo pipefail

[ $# -ge 1 ] || { echo "usage: $0 PATH..." >&2; exit 2; }

roots=()
for r in "${TMPDIR:-}" /tmp /private/tmp; do
  [ -n "$r" ] && [ -d "$r" ] && roots+=("$(cd "$r" && pwd -P)")
done

checked=()
for p in "$@"; do
  case $p in
    *'*'* | *'?'* | *'['*) echo "scratch-rm: refusing a glob: $p" >&2; exit 2 ;;
  esac
  case $(basename "$p") in
    . | ..) echo "scratch-rm: refusing a path ending in . or ..: $p" >&2; exit 2 ;;
  esac
  [ -e "$p" ] || [ -L "$p" ] || { echo "scratch-rm: no such path (skipped): $p"; continue; }
  parent=$(cd "$(dirname "$p")" && pwd -P)
  real="$parent/$(basename "$p")"
  ok=
  for r in "${roots[@]}"; do
    case $real in "$r"/?*) ok=1 ;; esac
  done
  [ -n "$ok" ] || { echo "scratch-rm: refusing a path outside the temp dirs: $p" >&2; exit 2; }
  for r in "${roots[@]}"; do
    [ "$real" = "$r" ] && { echo "scratch-rm: refusing a temp root itself: $p" >&2; exit 2; }
  done
  [ -e "$real/.git" ] && { echo "scratch-rm: $p is a git worktree; use git worktree remove --force" >&2; exit 2; }
  checked+=("$real")
done

for real in ${checked[@]+"${checked[@]}"}; do
  rm -rf -- "$real"
  echo "scratch-rm: removed $real"
done
