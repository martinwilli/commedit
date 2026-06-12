#!/bin/sh
# Launch the bundled commedit-mcp server for this OS/architecture.
#
# The plugin ships one prebuilt binary per supported target; this script picks
# the matching one and execs it, forwarding the repository path. It is invoked
# as `sh launch.sh <repo>` (see ../.mcp.json), so it needs no execute bit
# itself, and it chmod +x's the selected binary before exec — together that
# makes the plugin work even if the upload/install path drops Unix permissions.
#
# stdout is the MCP JSON-RPC channel and must stay clean: all diagnostics go to
# stderr.

set -eu

dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

os=$(uname -s)
case "$os" in
    Linux) os=linux ;;
    Darwin) os=macos ;;
    *) echo "commedit-mcp: unsupported OS '$os'" >&2; exit 1 ;;
esac

arch=$(uname -m)
case "$arch" in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *) echo "commedit-mcp: unsupported architecture '$arch'" >&2; exit 1 ;;
esac

bin="$dir/commedit-mcp-$os-$arch"
if [ ! -f "$bin" ]; then
    echo "commedit-mcp: no bundled binary for $os/$arch ($bin)" >&2
    exit 1
fi
# Self-heal the executable bit in case the distribution channel dropped it.
[ -x "$bin" ] || chmod +x "$bin" 2>/dev/null || true

# Forward the repository path only when it resolves to a real directory;
# otherwise let the binary fall back to its own default (the current directory).
# This guards against an empty or unexpanded ${CLAUDE_PROJECT_DIR}.
target=${1-}
if [ -n "$target" ] && [ -d "$target" ]; then
    exec "$bin" "$target"
fi
exec "$bin"
