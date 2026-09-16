#!/usr/bin/env bash
#
# Build lyv for release and link it into the project root. Arguments pass
# through to cargo.
#
#     ./build.sh                          the fast binary, at ./lyv
#     ./build.sh --features memory-stats  the same, reporting live bytes
#
set -euo pipefail

cd "$(dirname "$0")"

# Not `exec`: that would replace this shell and the link below would never
# happen. `set -e` still stops here if the build fails, so the link is only
# ever made over a binary that exists.
cargo build --release "$@"

# `./lyv` rather than typing `./target/release/lyv` every time — and a symlink
# rather than a copy, so it cannot go stale against the binary it names.
#
# Relative, so the link survives the directory being moved or copied. `-f` to
# replace an existing link, `-n` so that replacing one does not mean creating
# a new link *inside* it should it ever point at a directory.
ln -sfn target/release/lyv lyv
