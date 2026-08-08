#!/usr/bin/env bash
# Run the server against the library you already scanned on this machine,
# without copying anything into a container volume.
#
#   ALBUMPLAYER_PASSWORD=something ./run-dev-server.sh
#
# Then open http://127.0.0.1:8080
set -euo pipefail

if [[ -z "${ALBUMPLAYER_PASSWORD:-}" ]]; then
  echo "Set a password first, e.g.:" >&2
  echo "  ALBUMPLAYER_PASSWORD=your-password ./run-dev-server.sh" >&2
  exit 1
fi

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Point at the CLI's own database and cover cache rather than /data, so the
# scan and enrichment already done on this machine are used as-is.
export ALBUMPLAYER_DATA_DIR="${ALBUMPLAYER_DATA_DIR:-$HOME/.local/share/albumplayer}"
export ALBUMPLAYER_ART_DIR="${ALBUMPLAYER_ART_DIR:-$HOME/.cache/albumplayer/art}"
export ALBUMPLAYER_MUSIC_ROOT="${ALBUMPLAYER_MUSIC_ROOT:-/mnt/mozek/Home/Music}"
export ALBUMPLAYER_UI_DIR="${ALBUMPLAYER_UI_DIR:-$here/ui/dist}"
export ALBUMPLAYER_BIND="${ALBUMPLAYER_BIND:-127.0.0.1:8080}"
# The library is already scanned; skip the walk so startup is instant.
export ALBUMPLAYER_SCAN_ON_START="${ALBUMPLAYER_SCAN_ON_START:-false}"

exec "$here/target/release/albumplayer-server"
