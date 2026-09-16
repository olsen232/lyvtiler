#!/usr/bin/env bash
#
# Open a built archive in the viewer.
#
#     ./view.sh parcels.pmtiles
#
# A wrapper around `viewer/serve.py`, which serves the archive with working
# HTTP Range requests — `pmtiles.js` reads the header, a directory and then
# each tile by byte range, and a plain static server hands back the whole
# archive every time. Any arguments pass straight through.
#
set -euo pipefail

cd "$(dirname "$0")"

exec python3 viewer/serve.py "$@"
