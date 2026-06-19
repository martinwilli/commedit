#!/bin/sh
# commedit plugin — PostToolUse hook for EnterWorktree.
#
# The commedit MCP server is one process bound to the repository it opened; it
# does NOT follow the working directory or the harness's worktree. Entering a
# worktree therefore moves only the files — every commedit tool keeps acting on
# the ORIGINAL repo (reads as much as edits) until reload_repo(path=...) is
# called. This hook injects that reminder so the agent retargets the server
# before it reads or rewrites the wrong repo's history.
#
# Pure context injection: exit 0, a hookSpecificOutput.additionalContext object
# on stdout. The quoted heredoc keeps the JSON literal (no shell expansion, no
# escaping pitfalls), so this needs nothing beyond a POSIX shell.
cat <<'JSON'
{"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":"commedit does not follow your working directory. A git worktree was just entered, but the commedit MCP server is still bound to the repository it originally opened — so until you retarget it, EVERY commedit tool operates on the ORIGINAL repo, not this worktree. This holds for reads (list_history, show_commit, show_graph, session_diff, working_copy_status) just as much as for edits, so the history you see is also the wrong repo's. Before using any commedit tool here, call reload_repo with path set to this worktree's root and confirm the returned root matches; reload_repo onto the main checkout to switch back. See the commedit work-in-worktree skill."}}
JSON
