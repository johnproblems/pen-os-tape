#!/usr/bin/env python3
"""Render one os-tape demo video from a genuine `script` typescript.

Every terminal line shown is read verbatim from a typescript produced by
running the real commands under `script(1)`. This file only reveals those
lines progressively over 12 s; it never invents command output.

Usage: render-demos.py [--ffmpeg FFMPEG] [--font FONT] TYPESCRIPT MP4 TITLE
"""
import argparse
import subprocess
import sys
import tempfile
from pathlib import Path

W, H = 1280, 720
FONTSIZE = 16
LINE_H = 22
X0, Y0 = 40, 72
DUR = 12.0
FPS = 10

GREEN = "#7ee787"
RED = "#ff7b72"
GRAY = "#e6e6e6"
DIM = "#8b949e"
TITLE_BG = "#0d1117"
BG = "#141414"


def load_lines(ts: Path, width: int = 78):
    out = []
    for raw in ts.read_bytes().split(b"\n"):
        line = raw.decode("utf-8", "replace").replace("\r", "")
        if line.startswith("Script started") or line.startswith("Script done"):
            continue
        line = line.expandtabs(8)
        if line == "" and (not out or out[-1] == ""):
            continue
        while len(line) > width:
            cut = line.rfind(" ", 0, width)
            if cut <= 0:
                cut = width
            out.append(line[:cut])
            line = "  " + line[cut:].lstrip()
        out.append(line)
    while out and out[-1] == "":
        out.pop()
    while out and out[0] == "":
        out.pop(0)
    return out


def color_for(line: str) -> str:
    if line.startswith("$ "):
        return GREEN
    if line.startswith("+") and not line.startswith("+++"):
        return GREEN
    if line.startswith("PASS"):
        return GREEN
    if (line.startswith("-") and not line.startswith("---")) or "FAIL" in line:
        return RED
    if line.startswith("os-tape:"):
        return DIM
    return GRAY


def render(ffmpeg: str, font: str, ts: Path, mp4: Path, title: str):
    lines = load_lines(ts)
    n = len(lines)
    if n * LINE_H + Y0 > H:
        raise SystemExit(f"{n} lines overflow {H}px canvas")
    with tempfile.TemporaryDirectory(prefix="demoframes") as tmp:
        work = Path(tmp)
        (work / "title.txt").write_text(title)
        for i, line in enumerate(lines):
            (work / f"line{i:02d}.txt").write_text(line if line else " ")
        t0, t1 = 0.8, DUR - 1.5
        filt = [f"drawbox=y=0:w={W}:h=54:c={TITLE_BG}:t=fill"]
        filt.append(
            f"drawtext=fontfile={font}:textfile={work / 'title.txt'}:fontsize=18:"
            f"fontcolor={DIM}:x={X0}:y=16:expansion=none"
        )
        for i, line in enumerate(lines):
            ti = t0 + (t1 - t0) * i / max(n - 1, 1)
            filt.append(
                f"drawtext=fontfile={font}:textfile={work / f'line{i:02d}.txt'}:"
                f"fontsize={FONTSIZE}:fontcolor={color_for(line)}:"
                f"x={X0}:y={Y0 + i * LINE_H}:expansion=none:enable='gte(t,{ti:.3f})'"
            )
        cmd = [ffmpeg, "-y", "-f", "lavfi", "-i",
               f"color=c={BG}:s={W}x{H}:d={DUR}:r={FPS}",
               "-vf", ",".join(filt), "-c:v", "libx264", "-pix_fmt", "yuv420p",
               "-crf", "20", "-preset", "veryfast", str(mp4)]
        p = subprocess.run(cmd, capture_output=True, text=True)
        if p.returncode != 0:
            print(p.stderr[-3000:], file=sys.stderr)
            raise SystemExit("ffmpeg mp4 render failed")
    print(f"wrote {mp4} ({n} lines)")


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--ffmpeg", default="ffmpeg")
    ap.add_argument("--font", required=True)
    ap.add_argument("typescript")
    ap.add_argument("mp4")
    ap.add_argument("title")
    a = ap.parse_args()
    render(a.ffmpeg, a.font, Path(a.typescript), Path(a.mp4), a.title)
