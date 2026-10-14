#!/usr/bin/env bash
# Regenerate demos/demo-*.mp4 and demos/demo-*.gif from scratch.
#
# Every frame shows real output: the script builds the demo apps with `pen
# build`, runs the exact commands shown under `script(1)`, then reveals the
# recorded typescript lines progressively over 12 s. Nothing is mocked.
#
# Requirements: pen + LLVM 16 on PATH, PEN_ROOT pointing at a Pen checkout,
# ffmpeg/ffprobe on PATH (or FFBIN pointing at their bin dir), a monospace
# font visible to fc-match, and `script(1)` (util-linux).
#
# Usage: scripts/make-demos.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FFBIN="${FFBIN:-}"
if [ -n "$FFBIN" ]; then
  export PATH="$FFBIN:$PATH"
fi
for c in pen script ffmpeg ffprobe fc-match; do
  command -v "$c" >/dev/null || { echo "make-demos: need $c on PATH" >&2; exit 2; }
done
[ -n "${PEN_ROOT:-}" ] || { echo "make-demos: PEN_ROOT must point at a Pen checkout" >&2; exit 2; }
FONT="$(fc-match --format='%{file}\n' 'DejaVu Sans Mono' | head -1)"
[ -f "${FONT:-}" ] || FONT="$(fc-match --format='%{file}\n' monospace | head -1)"
[ -f "${FONT:-}" ] || { echo "make-demos: no monospace font found" >&2; exit 2; }

WORK="$(mktemp -d /tmp/os-tape-demos.XXXXXX)"
trap 'rm -rf "$WORK"' EXIT
export TERM=xterm COLUMNS=80

stage_console_cat() {
  for n in console cat; do cp -r "$PEN_ROOT/examples/$n" "$WORK/$n"; done
}
point_os_at_repo() {
  python3 - "$1" <<EOF
import json, sys
p = sys.argv[1] + "/pen.json"
c = json.load(open(p)); c["dependencies"]["Os"] = "file://$ROOT"
json.dump(c, open(p, "w"), indent=2)
EOF
}
mkfixture() { # name + main.pen on stdin
  mkdir -p "$WORK/$1"
  cat > "$WORK/$1/main.pen"
  python3 -c "import json; json.dump({'type':'application','dependencies':{'Console':'../console','Os':'file://$ROOT'}}, open('$WORK/$1/pen.json','w'), indent=2)"
}

stage_console_cat
point_os_at_repo "$WORK/console"
point_os_at_repo "$WORK/cat"
mkfixture sleep <<'EOF'
import Console'Print
import Os'Time

main = \(ctx context) none {
  Time'Sleep(ctx.Os, 10000)

  _ = Print'Line(ctx.Os, "slept")

  none
}
EOF
mkfixture w1 <<'EOF'
import Console'Print

main = \(ctx context) none {
  _ = Print'Line(ctx.Os, "line1")

  none
}
EOF
mkfixture w2 <<'EOF'
import Console'Print

main = \(ctx context) none {
  _ = Print'Line(ctx.Os, "line1 changed")

  none
}
EOF
for d in cat sleep w1 w2; do
  echo "building $d ..."
  (cd "$WORK/$d" && pen build >/dev/null) || { echo "make-demos: pen build failed in $d" >&2; exit 1; }
done

mkdir -p "$WORK/demo1" "$WORK/demo2"
cp "$WORK/cat/app" "$WORK/demo1/cat-app"
cp "$WORK/sleep/app" "$WORK/demo2/sleep-app"
cp "$WORK/w1/app" "$WORK/demo2/w1"
cp "$WORK/w2/app" "$WORK/demo2/w2"
cat > "$WORK/run1.sh" <<EOF
#!/bin/bash
export PATH="$ROOT/bin:\$PATH"
run() { printf '\$ %s\n' "\$1"; eval "\$1" 2>&1; }
run "printf 'hello tape\n' > hello.txt"
run "cat hello.txt"
run "PEN_TAPE=record:cat.tape ./cat-app hello.txt"
run "rm hello.txt"
run "PEN_TAPE=replay:cat.tape ./cat-app hello.txt"
run "os-tape info cat.tape"
run "os-tape validate cat.tape"
EOF
cat > "$WORK/run2.sh" <<EOF
#!/bin/bash
export PATH="$ROOT/bin:\$PATH"
run() { printf '\$ %s\n' "\$1"; eval "\$1" 2>&1; }
run "time PEN_TAPE=record:sleep.tape ./sleep-app"
run "time PEN_TAPE=replay:sleep.tape ./sleep-app"
run "PEN_TAPE=record:out.tape ./w1"
run 'PEN_TAPE=replay:out.tape PEN_TAPE_OUTPUTS=diff ./w2; echo "diff exit=\$?"'
run "mkdir -p tapes && cp out.tape tapes/ && os-tape check tapes/"
EOF
(cd "$WORK/demo1" && script --return --quiet -c "bash $WORK/run1.sh" "$WORK/demo1.typescript" >/dev/null)
(cd "$WORK/demo2" && script --return --quiet -c "bash $WORK/run2.sh" "$WORK/demo2.typescript" >/dev/null)

mkdir -p "$ROOT/demos"
python3 "$ROOT/scripts/render-demos.py" --ffmpeg "$(command -v ffmpeg)" --font "$FONT" \
  "$WORK/demo1.typescript" "$ROOT/demos/demo-record-replay.mp4" "os-tape demo 1/2: record once, replay exactly"
python3 "$ROOT/scripts/render-demos.py" --ffmpeg "$(command -v ffmpeg)" --font "$FONT" \
  "$WORK/demo2.typescript" "$ROOT/demos/demo-diff.mp4" "os-tape demo 2/2: instant replay + output diff"
for d in demo-record-replay demo-diff; do
  ffmpeg -y -v error -i "$ROOT/demos/$d.mp4" \
    -vf "fps=10,scale=960:-1:flags=lanczos,palettegen" "$WORK/$d.pal.png"
  ffmpeg -y -v error -i "$ROOT/demos/$d.mp4" -i "$WORK/$d.pal.png" \
    -lavfi "fps=10,scale=960:-1:flags=lanczos[x];[x][1:v]paletteuse" "$ROOT/demos/$d.gif"
done

echo "verifying ..."
for f in "$ROOT"/demos/demo-*.mp4 "$ROOT"/demos/demo-*.gif; do
  dur="$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$f")"
  python3 - "$dur" <<'EOF'
import sys
d = float(sys.argv[1])
sys.exit(0 if 10.0 <= d <= 15.0 else 1)
EOF
  [ $? -eq 0 ] || { echo "make-demos: $f duration $dur outside 10-15s" >&2; exit 1; }
  prev=""
  for t in 0.2 4 8 11.5; do
    h="$(ffmpeg -v error -ss "$t" -i "$f" -frames:v 1 -f md5 -)"
    [ "$h" != "$prev" ] || { echo "make-demos: $f static between frames" >&2; exit 1; }
    prev="$h"
  done
  echo "ok: $f (${dur}s, animated)"
done
echo "demos written to $ROOT/demos/"
