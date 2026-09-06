#!/bin/bash
# Restores a personal CLAUDE.local.md on Claude Code on the web sessions.
#
# CLAUDE.local.md is gitignored (per-developer, never committed), so a fresh
# web session has no copy of it. This hook recreates it from a per-environment
# secret so personal instructions still apply on the web without ever living
# in the repo.
#
# Setup (once per Claude Code on the web environment):
#   base64 -w0 CLAUDE.local.md
# Paste the output as the CLAUDE_LOCAL_MD_B64 environment variable/secret in
# the environment's settings on claude.ai/code.
set -euo pipefail

if [ "${CLAUDE_CODE_REMOTE:-}" != "true" ]; then
  exit 0
fi

if [ -z "${CLAUDE_LOCAL_MD_B64:-}" ]; then
  exit 0
fi

echo "$CLAUDE_LOCAL_MD_B64" | base64 -d > "$CLAUDE_PROJECT_DIR/CLAUDE.local.md"
