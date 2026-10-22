#!/usr/bin/env bash
# launch.sh — launch claude or agy in a tmux session in this directory.
# Reconnects if the session already exists. Use -r to force restart,
# and pass 'claude' (default) or 'agy' (or -a claude|agy) to select the agent.
set -euo pipefail

SESSION="ultraroute"
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLAUDE_CMD="claude --dangerously-skip-permissions"
AGY_CMD="agy --dangerously-skip-permissions"
AGENT="claude"
FORCE_RESTART=false

while getopts "ra:" opt; do
  case "$opt" in
    r) FORCE_RESTART=true ;;
    a) AGENT="$OPTARG" ;;
    *) echo "Usage: $0 [-r] [-a claude|agy] [claude|agy]"; exit 1 ;;
  esac
done
shift $((OPTIND - 1))
if [[ $# -gt 0 ]]; then
  AGENT="$1"
fi

case "$AGENT" in
  claude)
    AGENT_CMD="$CLAUDE_CMD"
    ;;
  agy)
    SESSION="${SESSION}-agy"
    AGENT_CMD="$AGY_CMD"
    ;;
  *)
    echo "Unknown agent '$AGENT' (expected: claude or agy)" >&2
    exit 1
    ;;
esac

# `=` means an exact session name. A bare target is a *prefix* match, and this
# machine has sibling projects whose names start with each other's (finance and
# finance-llm, conversational_agent and conversational_agent-stt). Unanchored,
# `finance/launch.sh` finds "finance-llm" when its own session is not running,
# attaches you to that project's agent, and with -r kills it.
if $FORCE_RESTART && tmux has-session -t "=$SESSION" 2>/dev/null; then
  tmux kill-session -t "=$SESSION"
fi

# Create session if it doesn't exist. `exec bash` keeps the pane alive if the agent exits.
if ! tmux has-session -t "=$SESSION" 2>/dev/null; then
  tmux new-session -d -s "$SESSION" -c "$DIR" "$AGENT_CMD; exec bash"
  echo "Created tmux session '$SESSION' in $DIR"
fi

# Attach (or switch client if already inside tmux)
if [[ -n "${TMUX:-}" ]]; then
  exec tmux switch-client -t "=$SESSION"
else
  exec tmux attach-session -t "=$SESSION"
fi
