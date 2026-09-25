#!/usr/bin/env bash
set -euo pipefail

# Minimal DBX Web API automation example.
# Requires a running DBX Web/Docker instance, curl and jq.
#
# Usage:
#   export DBX_WEB_PASSWORD='<your DBX Web password>'   # never hard-code it here
#   DBX_WEB_URL=http://localhost:4224 ./automation.sh

BASE_URL="${DBX_WEB_URL:-http://localhost:4224}"
: "${DBX_WEB_PASSWORD:?Set DBX_WEB_PASSWORD to your DBX Web password}"
COOKIE_JAR="$(mktemp)"
trap 'rm -f "$COOKIE_JAR"' EXIT

echo "==> Checking auth state"
curl -fsS "$BASE_URL/api/auth/check"

echo
echo "==> Logging in"
# jq builds the JSON (correct escaping) from the environment, and curl reads the
# body from stdin, so the password never appears on a command line (ps output).
jq -n '{password: env.DBX_WEB_PASSWORD}' \
  | curl -fsS -c "$COOKIE_JAR" \
      -H "Content-Type: application/json" \
      --data-binary @- \
      "$BASE_URL/api/auth/login" >/dev/null

echo "==> Listing saved connections"
curl -fsS -b "$COOKIE_JAR" "$BASE_URL/api/connection/list" | jq .

echo
echo "Done. Reuse the session cookie for schema and query routes."
