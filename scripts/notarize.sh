#!/usr/bin/env bash
# Sends a signed .zip or .dmg to Apple's notary service and waits for the verdict;
# used by .github/workflows/release.yml. On rejection, prints Apple's log and fails.
#
#   scripts/notarize.sh <file>
#
# Needs an App Store Connect API key: APPLE_API_KEY (the text of the AuthKey_….p8
# file), APPLE_API_KEY_ID and APPLE_API_ISSUER.
set -euo pipefail

file=$1
key=$(mktemp "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/notary.XXXXXX")
trap 'rm -f "$key"' EXIT
printf '%s\n' "$APPLE_API_KEY" > "$key"
auth=(--key "$key" --key-id "$APPLE_API_KEY_ID" --issuer "$APPLE_API_ISSUER")

result=$(xcrun notarytool submit "$file" "${auth[@]}" --wait --timeout 1h --output-format json)
echo "$result"
id=$(jq -r .id <<< "$result")
status=$(jq -r .status <<< "$result")
if [ "$status" != "Accepted" ]; then
  xcrun notarytool log "$id" "${auth[@]}" || true
  echo "::error::Notarizing $(basename "$file") ended with: $status"
  exit 1
fi
