#!/usr/bin/env python3
"""Serve a PMTiles archive and open it in a browser.

    python3 viewer/serve.py out.pmtiles

Nothing else in the repo depends on this, and it depends on nothing but the
standard library. It exists so that a result can be looked at rather than
inferred from a test.

What it does that a plain static server does not:

  - **HTTP Range requests.** `pmtiles.js` reads the header, then a directory,
    then individual tiles, all by byte range. `http.server` ignores `Range` and
    returns the whole file with a 200, which makes the client re-download the
    archive for every tile. Ranges are implemented here properly.
  - **Serves the page and a small config.** The page reads everything else —
    layers, bounds, zooms — from the archive itself.
"""

from __future__ import annotations

import argparse
import gzip
import http.server
import json
import mimetypes
import re
import socketserver
import sys
import threading
import webbrowser
from pathlib import Path

HERE = Path(__file__).resolve().parent
ARCHIVE_PATH = "/tiles.pmtiles"
FAVICON_PATH = "/favicon.png"
FAVICON_FILE = HERE.parent / "docs" / "lyv-icon.png"


def header_bounds(archive: Path) -> list[float] | None:
    """The extent the archive declares, or `None` if it isn't PMTiles v3.

    Fixed fields in the 127-byte PMTiles v3 header:

        102 min_lon_e7   106 min_lat_e7   110 max_lon_e7   114 max_lat_e7
    """
    with archive.open("rb") as handle:
        head = handle.read(127)
    if len(head) < 127 or head[:7] != b"PMTiles" or head[7] != 3:
        return None

    def e7(offset: int) -> float:
        return int.from_bytes(head[offset:offset + 4], "little", signed=True) / 1e7

    return [e7(102), e7(106), e7(110), e7(114)]


def adjusted_bounds(archive: Path) -> list[float] | None:
    """`antimeridian_adjusted_bounds` from the metadata JSON, if present.

    The metadata's offset and length are at header bytes 24 and 32.
    """
    with archive.open("rb") as handle:
        head = handle.read(127)
        if len(head) < 127 or head[:7] != b"PMTiles" or head[7] != 3:
            return None
        offset = int.from_bytes(head[24:32], "little")
        length = int.from_bytes(head[32:40], "little")
        handle.seek(offset)
        blob = handle.read(length)
    if blob[:2] == b"\x1f\x8b":
        blob = gzip.decompress(blob)
    text = json.loads(blob).get("antimeridian_adjusted_bounds")
    if not isinstance(text, str):
        return None
    return [float(part) for part in text.split(",")]


class Handler(http.server.BaseHTTPRequestHandler):
    """Serves the bundled page, a small config blob, and the archive."""

    archive: Path
    config: dict

    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):  # noqa: A003 - quieter than the default
        if self.server.verbose:
            super().log_message(fmt, *args)

    # -- routing ---------------------------------------------------------

    def do_HEAD(self):
        self._respond(head_only=True)

    def do_GET(self):
        self._respond(head_only=False)

    def _respond(self, head_only: bool):
        path = self.path.split("?", 1)[0]
        if path == "/":
            self._send_file(HERE / "index.html", head_only)
        elif path == "/config.json":
            body = json.dumps(self.config).encode()
            self._send_bytes(body, "application/json", head_only)
        elif path == FAVICON_PATH and FAVICON_FILE.is_file():
            self._send_file(FAVICON_FILE, head_only)
        elif path == ARCHIVE_PATH:
            self._send_range(self.archive, head_only)
        else:
            self.send_error(404, "not found")

    # -- sending ---------------------------------------------------------

    def _send_bytes(self, body: bytes, kind: str, head_only: bool):
        self.send_response(200)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if not head_only:
            self.wfile.write(body)

    def _send_file(self, path: Path, head_only: bool):
        kind = mimetypes.guess_type(path.name)[0] or "application/octet-stream"
        self._send_bytes(path.read_bytes(), kind, head_only)

    def _send_range(self, path: Path, head_only: bool):
        """Serve the archive, honouring `Range` the way PMTiles needs.

        A 206 with a correct `Content-Range` is the whole point: without it
        `pmtiles.js` still works, but fetches the entire archive for every
        directory lookup and every tile.
        """
        total = path.stat().st_size
        requested = self.headers.get("Range")
        start, end = 0, total - 1
        partial = False

        if requested:
            match = re.fullmatch(r"bytes=(\d*)-(\d*)", requested.strip())
            if not match:
                self.send_response(416)
                self.send_header("Content-Range", f"bytes */{total}")
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            first, last = match.group(1), match.group(2)
            if first == "" and last == "":
                self.send_error(400, "malformed Range")
                return
            if first == "":
                # A suffix range: the final N bytes.
                length = min(int(last), total)
                start, end = total - length, total - 1
            else:
                start = int(first)
                end = int(last) if last else total - 1
            if start >= total or start > end:
                self.send_response(416)
                self.send_header("Content-Range", f"bytes */{total}")
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            end = min(end, total - 1)
            partial = True

        length = end - start + 1
        self.send_response(206 if partial else 200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Content-Length", str(length))
        if partial:
            self.send_header("Content-Range", f"bytes {start}-{end}/{total}")
        self.end_headers()
        if head_only:
            return
        with path.open("rb") as handle:
            handle.seek(start)
            remaining = length
            while remaining > 0:
                chunk = handle.read(min(65536, remaining))
                if not chunk:
                    break
                self.wfile.write(chunk)
                remaining -= len(chunk)


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True
    verbose = False


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("archive", type=Path, help="a .pmtiles file to look at")
    parser.add_argument(
        "--layer",
        help="source-layer to style; defaults to the archive's first declared layer",
    )
    parser.add_argument("--port", type=int, default=8787)
    parser.add_argument(
        "--no-open", action="store_true", help="do not launch a browser"
    )
    parser.add_argument("--verbose", action="store_true", help="log every request")
    args = parser.parse_args()

    archive = args.archive.expanduser().resolve()
    if not archive.is_file():
        print(f"{archive} does not exist", file=sys.stderr)
        return 1

    bounds = header_bounds(archive)
    adjusted = adjusted_bounds(archive)

    Handler.archive = archive
    Handler.config = {
        "layer": args.layer,
        "stem": archive.name.split(".")[0],
        "name": archive.name,
        "archive": ARCHIVE_PATH,
    }

    try:
        server = Server(("127.0.0.1", args.port), Handler)
    except OSError as error:
        print(f"could not listen on port {args.port}: {error}", file=sys.stderr)
        return 1
    server.verbose = args.verbose

    url = f"http://127.0.0.1:{args.port}/"
    size = archive.stat().st_size
    print(f"serving {archive.name} ({size:,} bytes)")
    if args.layer:
        print(f"  layer: {args.layer}")
    if adjusted:
        print("  data:  {:.4f},{:.4f} to {:.4f},{:.4f} (across the antimeridian)".format(*adjusted))
    elif bounds:
        print("  data:  {:.4f},{:.4f} to {:.4f},{:.4f}".format(*bounds))
    print(f"  at:    {url}")
    print("  stop with ctrl-c", flush=True)

    if not args.no_open:
        threading.Timer(0.3, lambda: webbrowser.open(url)).start()

    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print()
    finally:
        server.shutdown()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
