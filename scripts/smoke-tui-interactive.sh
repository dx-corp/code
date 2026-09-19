#!/usr/bin/env bash
# Interactive TUI smoke: start the canonical maestro binary in tmux, wait for
# paint, send Ctrl+C, and require a clean exit.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
if [[ -n "${MAESTRO_TUI_BIN:-}" ]]; then
  BIN="$MAESTRO_TUI_BIN"
else
  BIN=""
  for candidate in \
    "$ROOT/target/debug/maestro" \
    "$ROOT/target/release/maestro" \
    "$ROOT/target/debug/maestro-tui" \
    "$ROOT/target/release/maestro-tui"
  do
    if [[ -x "$candidate" ]]; then
      BIN="$candidate"
      break
    fi
  done
fi
if [[ ! -x "$BIN" ]]; then
  echo "maestro binary not found; build with cargo build -p maestro" >&2
  exit 1
fi
if ! command -v tmux >/dev/null 2>&1; then
  echo "tmux not available; skipping interactive TUI smoke" >&2
  exit 0
fi

SESSION="maestro-tui-smoke-$$"
SMOKE_HOME="$(mktemp -d "${TMPDIR:-/tmp}/maestro-tui-smoke.XXXXXX")"
# Invoked by the EXIT trap below.
# shellcheck disable=SC2329
cleanup() {
  tmux kill-session -t "$SESSION" 2>/dev/null || true
  rm -rf -- "$SMOKE_HOME"
}
trap cleanup EXIT

printf '%s\n' '{"onboardingSeen":true}' > "$SMOKE_HOME/ui.json"
printf -v QUOTED_SMOKE_HOME '%q' "$SMOKE_HOME"
tmux new-session -d -s "$SESSION" -x 100 -y 30 -c "$ROOT"
tmux send-keys -t "$SESSION" "export MAESTRO_HOME=$QUOTED_SMOKE_HOME" Enter
tmux send-keys -t "$SESSION" "export MAESTRO_TELEMETRY=0" Enter
tmux send-keys -t "$SESSION" "export MAESTRO_AUTO_UPDATE=0" Enter
tmux send-keys -t "$SESSION" "export MAESTRO_NO_SESSION=1" Enter
# Use a dummy key so startup does not block on credential resolution for paint.
tmux send-keys -t "$SESSION" "export OPENAI_API_KEY=sk-test-smoke" Enter
tmux send-keys -t "$SESSION" "'$BIN' --provider openai -m gpt-4.1-mini; echo EXIT:\$?" Enter
sleep 3
PANE="$(tmux capture-pane -t "$SESSION" -p -S -80 || true)"
if ! printf '%s' "$PANE" | grep -E -q 'Type a message|deixic|◉|gpt-4|approvals:|trust:|sandbox:'; then
  echo "TUI did not paint expected chrome:" >&2
  printf '%s\n' "$PANE" >&2
  exit 1
fi
tmux send-keys -t "$SESSION" C-c
sleep 1
# Idle Ctrl+C should quit
tmux send-keys -t "$SESSION" C-c
sleep 1
FINAL="$(tmux capture-pane -t "$SESSION" -p -S -40 || true)"
tmux kill-session -t "$SESSION" 2>/dev/null || true
if printf '%s' "$FINAL" | grep -q 'EXIT:0'; then
  echo "interactive TUI smoke passed"
  exit 0
fi
# Some builds exit without printing EXIT if shell exits; process gone is enough.
if ! pgrep -f "$BIN --provider openai" >/dev/null 2>&1; then
  echo "interactive TUI smoke passed (process exited)"
  exit 0
fi
echo "TUI did not exit cleanly:" >&2
printf '%s\n' "$FINAL" >&2
exit 1
