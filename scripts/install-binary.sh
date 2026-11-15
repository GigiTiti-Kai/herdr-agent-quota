#!/bin/sh
# Copy the release build to bin/, where `cargo clean` and target/ cleanups do
# not reach it. A temporary file and a rename replace a running binary without
# "Text file busy"; the new mtime is what makes a running watcher re-exec.
set -eu
root="$(cd "$(dirname "$0")/.." && pwd)"
mkdir -p "$root/bin"
tmp="$root/bin/.herdr-agent-usage.$$"
cp "$root/target/release/herdr-agent-usage" "$tmp"
mv -f "$tmp" "$root/bin/herdr-agent-usage"
