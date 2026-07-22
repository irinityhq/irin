#!/bin/bash
# Validate nginx config before deploy
# Usage: bash test/validate.sh
set -euo pipefail

echo "Validating nginx.conf..."
docker compose run --rm --no-deps gateway openresty -t 2>&1
echo "✅ Config valid"
