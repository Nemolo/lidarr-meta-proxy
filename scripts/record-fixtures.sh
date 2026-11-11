#!/usr/bin/env sh
# Records real responses from the upstream metadata server, Deezer and MusicBrainz
# for a few sample entities into tests/fixtures/recorded/, to refresh the shapes the
# hand-written mocks in tests/common are modelled on.
#
#   ./scripts/record-fixtures.sh
set -eu
out="$(dirname "$0")/../tests/fixtures/recorded"
mkdir -p "$out"
UA="${MB_USER_AGENT:-lidarr-meta-proxy-fixtures/0.1 ( https://github.com/nemolo/lidarr-meta-proxy )}"
UP="${UPSTREAM_URL:-https://api.lidarr.audio/api/v0.4}"

fetch() { # name url
    echo "-> $1"
    curl -fsS -A "$UA" "$2" -o "$out/$1.json" || echo "   failed: $2"
    sleep 1.1 # MusicBrainz: max 1 request per second
}

# Daft Punk (MB artist with a Deezer link).
fetch upstream-artist-daftpunk "$UP/artist/056e4f3e-d505-4dad-8ec1-d04f521cbb56"
fetch upstream-search-artist "$UP/search?type=artist&query=daft%20punk"
fetch upstream-recent-artist "$UP/recent/artist?since=$(($(date +%s) - 3600))"
fetch deezer-artist-27 "https://api.deezer.com/artist/27"
fetch deezer-artist-27-albums "https://api.deezer.com/artist/27/albums?limit=100"
fetch deezer-album-302127 "https://api.deezer.com/album/302127"
fetch deezer-album-302127-tracks "https://api.deezer.com/album/302127/tracks?limit=100"
fetch deezer-search-artist "https://api.deezer.com/search/artist?q=daft%20punk&limit=5"
fetch mb-url-deezer-artist-27 "https://musicbrainz.org/ws/2/url?resource=https://www.deezer.com/artist/27&inc=artist-rels+release-rels&fmt=json"
fetch mb-artist-releases "https://musicbrainz.org/ws/2/release?artist=056e4f3e-d505-4dad-8ec1-d04f521cbb56&inc=release-groups&limit=100&fmt=json"
fetch itunes-lookup-upc "https://itunes.apple.com/lookup?upc=724384960650&entity=album&country=it"
echo "done: $out"
