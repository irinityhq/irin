#!/bin/bash
# IRIN — shared env for launchd services.
# Provider credentials are inherited from the login shell. Never hardcode or
# copy them into this script.

# Base PATH (before NVM)
export PATH="$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"

# NVM (for node/npm) — must come AFTER base PATH so NVM prepends and wins
export NVM_DIR="$HOME/.nvm"
[ -s "$NVM_DIR/nvm.sh" ] && source "$NVM_DIR/nvm.sh" 2>/dev/null
export PYTHONUNBUFFERED=1

# Loopback-only dev: bypass auth (backend only listens on 127.0.0.1)
export COUNCIL_DEV_NO_AUTH=1
