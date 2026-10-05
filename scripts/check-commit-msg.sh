#!/usr/bin/env bash
# Rejects commit messages that credit anyone but the human author: no
# co-author trailers, no "Generated with ..." footers (see AGENTS.md).
#
#   scripts/check-commit-msg.sh FILE         check one message (commit-msg hook)
#   scripts/check-commit-msg.sh --range A..B check every commit in a range (CI)

set -euo pipefail

FORBIDDEN='^[[:space:]]*co-authored-by:|generated (with|by) \[?(claude|copilot|chatgpt|codex|cursor|gemini|an? ai)|🤖'

check_message() {
    local name="$1" message="$2"
    local hits
    hits="$(printf '%s\n' "$message" | grep -inE "$FORBIDDEN" || true)"
    if [[ -n $hits ]]; then
        echo "error: $name credits someone other than the author (see AGENTS.md):" >&2
        printf '%s\n' "$hits" | sed 's/^/    /' >&2
        return 1
    fi
}

if [[ ${1:-} == --range ]]; then
    range="${2:?usage: $0 --range A..B}"
    status=0
    for commit in $(git rev-list "$range"); do
        check_message "commit $(git rev-parse --short "$commit")" "$(git log -1 --format=%B "$commit")" || status=1
    done
    exit "$status"
fi

file="${1:?usage: $0 COMMIT_MSG_FILE | --range A..B}"
# Lines starting with '#' are git's comments, not part of the message.
check_message "this commit message" "$(grep -v '^#' "$file")"
