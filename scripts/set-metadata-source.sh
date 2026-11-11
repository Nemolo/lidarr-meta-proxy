#!/usr/bin/env sh
# Points Lidarr's (hidden) MetadataSource setting at the proxy, or resets it.
#
#   LIDARR_URL=http://localhost:8686 LIDARR_API_KEY=xxx ./scripts/set-metadata-source.sh http://lidarr-meta:5001
#   LIDARR_URL=http://localhost:8686 LIDARR_API_KEY=xxx ./scripts/set-metadata-source.sh ""   # back to the default
#
# Requires curl and jq. The API key is in Lidarr: Settings > General > Security.
set -eu

: "${LIDARR_URL:?set LIDARR_URL, e.g. http://localhost:8686}"
: "${LIDARR_API_KEY:?set LIDARR_API_KEY}"
SOURCE="${1-http://lidarr-meta:5001}"
endpoint="${LIDARR_URL%/}/api/v1/config/metadataprovider"

current=$(curl -fsS -H "X-Api-Key: $LIDARR_API_KEY" "$endpoint")
echo "current metadataSource: $(echo "$current" | jq -r '.metadataSource // ""')"

echo "$current" | jq --arg s "$SOURCE" '.metadataSource = $s' |
    curl -fsS -X PUT -H "X-Api-Key: $LIDARR_API_KEY" -H "Content-Type: application/json" \
        --data-binary @- "$endpoint" | jq -r '"new metadataSource: " + (.metadataSource // "")'
