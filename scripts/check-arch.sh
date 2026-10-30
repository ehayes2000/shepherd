#!/usr/bin/env bash
# Enforces the dependency rules in docs/architecture.md.
# Changing an allowlist here is an architecture decision: log it in docs/architecture.md.
set -euo pipefail
cd "$(dirname "$0")/.."

# External crates shepherd-core may use (normal deps only).
CORE_ALLOWED="serde serde_json thiserror jiff base64 sha2 toml unicode-segmentation unicode-normalization unicode-properties"
# External crates shepherd-tui may use beyond core's allowlist.
TUI_ALLOWED="ratatui crossterm unicode-width"
# External crates shepherd-term may use.
TERM_ALLOWED="alacritty_terminal"

# Internal crates each crate may depend on (normal + build deps). Unknown crates are not checked.
internal_allowed() {
  case "$1" in
    shepherd-core | shepherd-term) echo "" ;;
    shepherd-tui) echo "shepherd-core shepherd-term" ;;
    shepherd-adapters | shepherd-testkit) echo "shepherd-core" ;;
    shepherd) echo "shepherd-core shepherd-term shepherd-tui shepherd-adapters" ;;
    *) return 1 ;;
  esac
}

fail=0
err() { printf 'arch: %s\n' "$1" >&2; fail=1; }
in_list() { [[ " $2 " == *" $1 "* ]]; }

meta=$(cargo metadata --format-version 1 --no-deps)
internal=$(jq -r '.packages[].name' <<<"$meta" | tr '\n' ' ')

# name, dep, kind ("normal" | "dev" | "build")
while IFS=$'\t' read -r pkg dep kind; do
  allowed=$(internal_allowed "$pkg") || continue

  if in_list "$dep" "$internal"; then
    if [[ "$dep" == "shepherd-testkit" ]]; then
      [[ "$kind" == "dev" ]] || err "$pkg: shepherd-testkit must be a dev-dependency"
    elif [[ "$kind" != "dev" ]] && ! in_list "$dep" "$allowed"; then
      err "$pkg must not depend on $dep (allowed: ${allowed:-none})"
    fi
    continue
  fi

  [[ "$kind" == "dev" ]] && continue
  case "$pkg" in
    shepherd-core)
      in_list "$dep" "$CORE_ALLOWED" || err "shepherd-core: external dep '$dep' not in CORE_ALLOWED" ;;
    shepherd-tui)
      in_list "$dep" "$CORE_ALLOWED $TUI_ALLOWED" || err "shepherd-tui: external dep '$dep' not in TUI_ALLOWED" ;;
    shepherd-term)
      in_list "$dep" "$TERM_ALLOWED" || err "shepherd-term: external dep '$dep' not in TERM_ALLOWED" ;;
  esac
done < <(jq -r '.packages[] | .name as $p | .dependencies[] | [$p, .name, (.kind // "normal")] | @tsv' <<<"$meta")

# No silently skipped tests.
if grep -rn --include='*.rs' '#\[ignore' crates src 2>/dev/null; then
  err "#[ignore] is not allowed; fix or delete the test with justification"
fi

# Project docs agents depend on must never be empty or missing.
for doc in CLAUDE.md docs/architecture.md docs/acpp.md docs/plan.md docs/pm.md; do
  [[ -s "$doc" ]] || err "$doc is missing or empty"
done

# shepherd-tui draws text only through crate::draw, which takes a DisplayText (F35). crates/tui/clippy.toml bans
# ratatui's text methods and `Widget::render`; these are the `From` conversions clippy can't single out.
tui_from='(^|[^A-Za-z0-9_])(Line|Text|Span|ListItem|Row|Cell)::from\('
if tui_text=$(grep -rnE --include='*.rs' "$tui_from" crates/tui 2>/dev/null |
  grep -v '^crates/tui/src/draw\.rs:' | grep -vE '^[^:]+:[0-9]+:[[:space:]]*//'); then
  printf '%s\n' "$tui_text" >&2
  err "shepherd-tui: text must reach ratatui through crate::draw (a DisplayText), not Line/Text/Span/ListItem/Row/Cell::from"
fi

if [[ -d crates/core && ! -f crates/core/clippy.toml ]]; then
  err "crates/core/clippy.toml (purity lints) is missing"
fi

[[ $fail -eq 0 ]] && echo "architecture rules ok"
exit $fail
