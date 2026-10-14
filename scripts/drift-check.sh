#!/bin/sh
# Fail if the fork's .pen files drifted from upstream packages/os, or if the
# tape wrapper table drifted from the fork's own Context.
#
# Usage: scripts/drift-check.sh [upstream-ref]   (default: main)
set -e

cd "$(dirname "$0")/.."

ref="${1:-main}"
# UPSTREAM_BASE overrides the fetch location (tests point it at a local
# file:// tree); by default the .pen files are compared against upstream.
base="${UPSTREAM_BASE:-https://raw.githubusercontent.com/pen-lang/pen/$ref/packages/os}"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

fail=0

for f in \
  Context.pen Directory.pen Environment.pen File.pen Process.pen Tcp.pen \
  Time.pen Udp.pen normalFile.pen \
  Context/context.pen File/Metadata.pen File/OpenOptions.pen \
  Tcp/AcceptedStream.pen Tcp/Listener.pen Tcp/Stream.pen \
  Udp/Datagram.pen Udp/Socket.pen \
; do
  if ! curl -sL --max-time 60 --fail -o "$tmp/up.pen" "$base/$f"; then
    echo "drift-check: cannot fetch upstream $f (ref $ref)" >&2
    fail=1
    continue
  fi
  if ! cmp -s "$tmp/up.pen" "$f"; then
    echo "drift-check: $f differs from upstream packages/os@$ref" >&2
    fail=1
  fi
done

# The 32 foreign functions in Context/context.pen must match the tape table.
grep -o '_pen_os_[a-z_0-9]*' Context/context.pen | sed 's/^_pen_os_//' | sort -u > "$tmp/pen.txt"
grep -oE '^[[:space:]]+[a-z_0-9]+: (input|output),' ffi/library/src/tape.rs \
  | sed -E 's/^[[:space:]]+//; s/: .*//' | sort -u > "$tmp/tape.txt"
if ! diff -u "$tmp/pen.txt" "$tmp/tape.txt"; then
  echo "drift-check: tape table does not match Context foreign functions" >&2
  fail=1
fi

count="$(wc -l < "$tmp/tape.txt" | tr -d ' ')"
if [ "$count" != 32 ]; then
  echo "drift-check: expected 32 wrapped functions, found $count" >&2
  fail=1
fi

if [ "$fail" != 0 ]; then
  exit 1
fi
echo "drift-check: .pen files match upstream packages/os@$ref; 32/32 functions wrapped"
