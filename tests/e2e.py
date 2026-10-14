#!/usr/bin/env python3
"""End-to-end success criteria for os-tape (needs the Pen toolchain).

Stages the upstream examples (cat, ls, tcp-client, udp-client) plus small
fixtures from $PEN_ROOT, builds them against this os-tape checkout with no
change except the Os URL, then checks the PRD success criteria:

- record + replay with byte-identical stdout (inputs deleted, servers down)
- 10 s sleep replays in under 1 s
- a one-line output change is reported exactly by diff mode
- os-tape check passes on clean tapes and fails on changed output
- exit codes, repeated reads, reverse-order reads, recorded time/errors,
  tape version refusal, and the validate/info/cat tools on real tapes

Run from the repo root:  python3 tests/e2e.py
"""

import hashlib
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
OS_TAPE = REPO_ROOT / "bin" / "os-tape"
SLEEP_MAIN = """import Console'Print
import Os'Time

main = \\(ctx context) none {
  Time'Sleep(ctx.Os, 10000)

  _ = Print'Line(ctx.Os, "slept")

  none
}
"""

EXIT_MAIN = """import Os'Process

main = \\(ctx context) none {
  Process'Exit(ctx.Os, 7)
}
"""

READTWICE_MAIN = """import Console'Print
import Os'Context { Context }
import Os'Environment
import Os'File
import Os'Process

main = \\(ctx context) none {
  if e = run(ctx.Os) as none {
    none
  } else {
    _ = Print'Line(ctx.Os, if s = source(e) as string { s } else { "unknown error" })
    Process'Exit(ctx.Os, 1)
  }
}

run = \\(ctx Context) none | error {
  args = Environment'Arguments(ctx)

  if [f, ..._] = args {
    file = File'Open(ctx, f())?
    first = File'Read(ctx, file)?
    second = File'Read(ctx, file)?
    File'Write(ctx, File'StdOut(), "first:" + first + "|second:" + second)?

    none
  } else {
    error("usage: readtwice <file>")
  }
}
"""

READAB_MAIN = """import Console'Print
import Os'Context { Context }
import Os'Environment
import Os'File
import Os'Process

main = \\(ctx context) none {
  if e = run(ctx.Os) as none {
    none
  } else {
    _ = Print'Line(ctx.Os, if s = source(e) as string { s } else { "unknown error" })
    Process'Exit(ctx.Os, 1)
  }
}

run = \\(ctx Context) none | error {
  args = Environment'Arguments(ctx)

  if [a, ...rest] = args {
    if [b, ..._] = rest {
      fa = File'Open(ctx, a())?
      fb = File'Open(ctx, b())?
      sa = File'Read(ctx, fa)?
      sb = File'Read(ctx, fb)?
      File'Write(ctx, File'StdOut(), "A:" + sa + "|B:" + sb)?

      none
    } else {
      error("usage: readab <a> <b>")
    }
  } else {
    error("usage: readab <a> <b>")
  }
}
"""

# Same output as READAB_MAIN but opens and reads the two files in reverse
# order, so replaying one's tape with the other proves per-key FIFO matching.
READBA_MAIN = READAB_MAIN.replace(
    "      fa = File'Open(ctx, a())?\n      fb = File'Open(ctx, b())?\n"
    "      sa = File'Read(ctx, fa)?\n      sb = File'Read(ctx, fb)?\n",
    "      fb = File'Open(ctx, b())?\n      fa = File'Open(ctx, a())?\n"
    "      sb = File'Read(ctx, fb)?\n      sa = File'Read(ctx, fa)?\n",
).replace("usage: readab <a> <b>", "usage: readba <a> <b>")

GETTIME_MAIN = """import Console'Print
import Core'Number
import Os'Context { Context }
import Os'Process
import Os'Time

main = \\(ctx context) none {
  if e = run(ctx.Os) as none {
    none
  } else {
    _ = Print'Line(ctx.Os, if s = source(e) as string { s } else { "unknown error" })
    Process'Exit(ctx.Os, 1)
  }
}

run = \\(ctx Context) none | error {
  t = Time'Now(ctx)
  Print'Line(ctx, Number'String(t))?

  none
}
"""

ERRREAD_MAIN = """import Console'Print
import Os'Context { Context }
import Os'Environment
import Os'File { File }
import Os'File'OpenOptions { OpenOptions }
import Os'Process

main = \\(ctx context) none {
  if e = run(ctx.Os) as none {
    none
  } else {
    _ = Print'Line(ctx.Os, if s = source(e) as string { s } else { "unknown error" })
    Process'Exit(ctx.Os, 1)
  }
}

describe = \\(e error) string {
  if s = source(e) as string { s } else { "unknown error" }
}

run = \\(ctx Context) none | error {
  args = Environment'Arguments(ctx)

  if [f, ..._] = args {
    path = f()
    wo = File'OpenWithOptions(ctx, path, OpenOptions{...OpenOptions'Default(), Write: true})?
    m1 = if s = File'Read(ctx, wo) as string { "unexpected success" } else { describe(s) }
    m2 = if g = File'Open(ctx, path + ".does-not-exist") as File { "unexpected open" } else { describe(g) }
    Print'Line(ctx, "read-wo: " + m1)?
    Print'Line(ctx, "open-missing: " + m2)?

    none
  } else {
    error("usage: errread <file>")
  }
}
"""

# name -> (main.pen source, extra pen.json dependencies)
FIXTURES = {
    "exit-app": (EXIT_MAIN, {}),
    "readtwice": (READTWICE_MAIN, {"Console": "../console"}),
    "readab": (READAB_MAIN, {"Console": "../console"}),
    "readba": (READBA_MAIN, {"Console": "../console"}),
    "gettime": (GETTIME_MAIN, {"Console": "../console", "Core": "pen:///core"}),
    "errread": (ERRREAD_MAIN, {"Console": "../console"}),
}

WRITEOUT_MAIN = """import Console'Print
import Os'Context { Context }
import Os'Environment
import Os'File
import Os'File'OpenOptions { OpenOptions }
import Os'Process

main = \\(ctx context) none {
  if e = run(ctx.Os) as none {
    none
  } else {
    _ = Print'Line(ctx.Os, if s = source(e) as string { s } else { "unknown error" })
    Process'Exit(ctx.Os, 1)
  }
}

run = \\(ctx Context) none | error {
  args = Environment'Arguments(ctx)

  if [f, ..._] = args {
    out = File'OpenWithOptions(ctx, f(), OpenOptions{...OpenOptions'Default(), Write: true, Create: true, Truncate: true})?
<<WRITES>>
    none
  } else {
    error("usage: writeout <file>")
  }
}
"""

WRITEOUT_V1 = WRITEOUT_MAIN.replace(
    "<<WRITES>>", '    File\'Write(ctx, out, "line1\\n")?'
)
WRITEOUT_V2 = WRITEOUT_MAIN.replace(
    "<<WRITES>>", '    File\'Write(ctx, out, "line1\\nline2\\n")?'
)
WRITEOUT_V3 = WRITEOUT_MAIN.replace(
    "<<WRITES>>",
    '    File\'Write(ctx, out, "line1\\n")?\n    File\'Write(ctx, out, "extra\\n")?',
)

LOOPER_MAIN = """import Console'Print
import Os'Context { Context }
import Os'Environment
import Os'Process
import Os'Time

main = \\(ctx context) none {
  if e = run(ctx.Os) as none {
    none
  } else {
    _ = Print'Line(ctx.Os, if s = source(e) as string { s } else { "unknown error" })
    Process'Exit(ctx.Os, 1)
  }
}

run = \\(ctx Context) none | error {
  loop(ctx, Environment'Arguments(ctx))?
}

loop = \\(ctx Context, args [string]) none | error {
  if [a, ...rest] = args {
    Print'Line(ctx, a())?
    Time'Sleep(ctx, 100)
    loop(ctx, rest)?
  } else {
    none
  }
}
"""

# Nested 100x100 loops keep the recursion depth shallow while producing
# exactly 10,000 small stdout lines for the recording-size test.
MANYLINES_MAIN = """import Console'Print
import Os'Context { Context }
import Os'Process

main = \\(ctx context) none {
  if e = run(ctx.Os) as none {
    none
  } else {
    _ = Print'Line(ctx.Os, if s = source(e) as string { s } else { "unknown error" })
    Process'Exit(ctx.Os, 1)
  }
}

run = \\(ctx Context) none | error {
  outer(ctx, 100)?
}

outer = \\(ctx Context, n number) none | error {
  if n == 0 {
    none
  } else {
    inner(ctx, 100)?
    outer(ctx, n - 1)?
  }
}

inner = \\(ctx Context, n number) none | error {
  if n == 0 {
    none
  } else {
    Print'Line(ctx, "0123456789")?
    inner(ctx, n - 1)?
  }
}
"""

FAILURES: list = []


def check(name, condition, detail=""):
    status = "ok" if condition else "FAIL"
    print(f"[{status}] {name}" + (f": {detail}" if detail and not condition else ""))
    if not condition:
        FAILURES.append(name)


def run(cmd, env=None, cwd=None, timeout=120):
    merged = dict(os.environ)
    if env:
        merged.update(env)
    return subprocess.run(cmd, env=merged, cwd=cwd, capture_output=True, timeout=timeout)


def pen_build(package_dir: Path):
    proc = run(["pen", "build"], cwd=package_dir, timeout=600)
    if proc.returncode != 0:
        print(proc.stdout.decode(errors="replace"))
        print(proc.stderr.decode(errors="replace"), file=sys.stderr)
        raise RuntimeError(f"pen build failed in {package_dir}")
    app = package_dir / "app"
    if not app.exists():
        raise RuntimeError(f"no app binary in {package_dir}")
    return app


def build_extra_package(work: Path, name: str, main_src: str, extra_deps: dict) -> Path:
    """Stage and build one more app package inside an already-staged work dir."""
    dest = work / name
    if dest.exists():
        shutil.rmtree(dest)
    dest.mkdir()
    (dest / "main.pen").write_text(main_src)
    deps = dict(extra_deps)
    deps["Os"] = f"file://{REPO_ROOT}"
    (dest / "pen.json").write_text(
        json.dumps({"type": "application", "dependencies": deps}, indent=2) + "\n"
    )
    print(f"building {name} ...")
    return pen_build(dest)


def stage_examples(work: Path) -> dict:
    pen_root = Path(os.environ.get("PEN_ROOT", ""))
    if not (pen_root / "examples" / "cat" / "main.pen").exists():
        raise RuntimeError("PEN_ROOT must point at a pen checkout (examples/ missing)")
    staged = {}
    for name in ("console", "cat", "ls", "tcp-client", "udp-client"):
        dest = work / name
        shutil.copytree(pen_root / "examples" / name, dest)
        staged[name] = dest
    # The only change: the Os URL now points at this checkout.
    for name in ("console", "cat", "ls", "tcp-client", "udp-client"):
        path = staged[name] / "pen.json"
        config = json.loads(path.read_text())
        config["dependencies"]["Os"] = f"file://{REPO_ROOT}"
        path.write_text(json.dumps(config, indent=2) + "\n")
    sleep = work / "sleep"
    sleep.mkdir()
    (sleep / "main.pen").write_text(SLEEP_MAIN)
    (sleep / "pen.json").write_text(
        json.dumps(
            {
                "type": "application",
                "dependencies": {
                    "Console": "../console",
                    "Os": f"file://{REPO_ROOT}",
                },
            },
            indent=2,
        )
        + "\n"
    )
    staged["sleep"] = sleep
    assert READBA_MAIN != READAB_MAIN, "readba fixture must differ from readab"
    for name, (main_src, extra_deps) in FIXTURES.items():
        dest = work / name
        dest.mkdir()
        (dest / "main.pen").write_text(main_src)
        deps = dict(extra_deps)
        deps["Os"] = f"file://{REPO_ROOT}"
        (dest / "pen.json").write_text(
            json.dumps({"type": "application", "dependencies": deps}, indent=2) + "\n"
        )
        staged[name] = dest
    return staged


class TcpEcho:
    def __init__(self):
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.sock.bind(("127.0.0.1", 0))
        self.port = self.sock.getsockname()[1]
        self.sock.listen(5)
        self.thread = threading.Thread(target=self.serve, daemon=True)

    def start(self):
        self.thread.start()

    def serve(self):
        while True:
            try:
                conn, _ = self.sock.accept()
            except OSError:
                return
            with conn:
                try:
                    data = conn.recv(4096)
                    if data:
                        conn.sendall(b"echo:" + data)
                except OSError:
                    pass

    def stop(self):
        try:
            self.sock.close()
        except OSError:
            pass


class UdpEcho:
    """Dual-stack echo server: the client binds `localhost` (v6 or v4
    depending on the machine) and must be able to connect back over the
    same family, so we serve both on one port."""

    def __init__(self):
        self.socks = []
        for family, addr in (
            (socket.AF_INET, "127.0.0.1"),
            (socket.AF_INET6, "::1"),
        ):
            sock = socket.socket(family, socket.SOCK_DGRAM)
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            if family == socket.AF_INET6:
                sock.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
            self.socks.append(sock)
        self.socks[0].bind(("127.0.0.1", 0))
        self.port = self.socks[0].getsockname()[1]
        self.socks[1].bind(("::1", self.port))
        self.threads = [
            threading.Thread(target=self.serve, args=(sock,), daemon=True)
            for sock in self.socks
        ]

    def start(self):
        for thread in self.threads:
            thread.start()

    def serve(self, sock):
        while True:
            try:
                data, addr = sock.recvfrom(4096)
                sock.sendto(b"echo:" + data, addr)
            except OSError:
                return

    def stop(self):
        for sock in self.socks:
            try:
                sock.close()
            except OSError:
                pass


def test_cat(work, apps):
    fixture = work / "hello.txt"
    fixture.write_text("hello tape\n")
    tape = work / "cat.tape"
    rec = run(
        [str(apps["cat"]), str(fixture)],
        env={"PEN_TAPE": f"record:{tape}"},
        cwd=work,
    )
    check("cat records", rec.returncode == 0, f"exit={rec.returncode}")
    check("cat tape written", tape.exists() and tape.stat().st_size > 0)
    fixture.unlink()
    rep = run(
        [str(apps["cat"]), str(fixture)],
        env={"PEN_TAPE": f"replay:{tape}"},
        cwd=work,
    )
    check("cat replays after inputs deleted", rep.returncode == 0, rep.stderr.decode()[:300])
    check(
        "cat replay stdout byte-identical",
        rep.stdout == rec.stdout == b"hello tape\n",
        f"record={rec.stdout!r} replay={rep.stdout!r}",
    )


def test_ls(work, apps):
    target = work / "lsdir"
    target.mkdir()
    (target / "b.txt").write_text("b")
    (target / "a.txt").write_text("a")
    tape = work / "ls.tape"
    rec = run([str(apps["ls"]), str(target)], env={"PEN_TAPE": f"record:{tape}"}, cwd=work)
    check("ls records", rec.returncode == 0, f"exit={rec.returncode}")
    shutil.rmtree(target)
    rep = run([str(apps["ls"]), str(target)], env={"PEN_TAPE": f"replay:{tape}"}, cwd=work)
    check("ls replays after directory deleted", rep.returncode == 0, rep.stderr.decode()[:300])
    check("ls replay stdout byte-identical", rep.stdout == rec.stdout, f"{rec.stdout!r} vs {rep.stdout!r}")


def test_tcp_client(work, apps):
    server = TcpEcho()
    server.start()
    host = f"127.0.0.1:{server.port}"
    tape = work / "tcp.tape"
    rec = run(
        [str(apps["tcp-client"]), host, "ping"],
        env={"PEN_TAPE": f"record:{tape}"},
        cwd=work,
    )
    server.stop()
    check("tcp-client records", rec.returncode == 0, rec.stderr.decode()[:300])
    check("tcp-client got echo", rec.stdout == b"echo:ping\n", repr(rec.stdout))
    time.sleep(0.2)
    rep = run(
        [str(apps["tcp-client"]), host, "ping"],
        env={"PEN_TAPE": f"replay:{tape}"},
        cwd=work,
    )
    check("tcp-client replays with server down", rep.returncode == 0, rep.stderr.decode()[:300])
    check("tcp-client replay stdout byte-identical", rep.stdout == rec.stdout)


def test_udp_client(work, apps):
    server = UdpEcho()
    server.start()
    host = f"localhost:{server.port}"
    tape = work / "udp.tape"
    rec = run(
        [str(apps["udp-client"]), host, "ping"],
        env={"PEN_TAPE": f"record:{tape}"},
        cwd=work,
    )
    server.stop()
    check("udp-client records", rec.returncode == 0, rec.stderr.decode()[:300])
    check("udp-client got echo", rec.stdout == b"echo:ping\n", repr(rec.stdout))
    rep = run(
        [str(apps["udp-client"]), host, "ping"],
        env={"PEN_TAPE": f"replay:{tape}"},
        cwd=work,
    )
    check("udp-client replays with server down", rep.returncode == 0, rep.stderr.decode()[:300])
    check("udp-client replay stdout byte-identical", rep.stdout == rec.stdout)


def test_sleep(work, apps):
    tape = work / "sleep.tape"
    start = time.monotonic()
    rec = run([str(apps["sleep"])], env={"PEN_TAPE": f"record:{tape}"}, cwd=work, timeout=60)
    record_seconds = time.monotonic() - start
    check("sleep records (~10 s)", rec.returncode == 0 and record_seconds >= 9, f"{record_seconds:.1f}s")
    start = time.monotonic()
    rep = run([str(apps["sleep"])], env={"PEN_TAPE": f"replay:{tape}"}, cwd=work, timeout=60)
    replay_seconds = time.monotonic() - start
    check("sleep replays in under 1 s", rep.returncode == 0 and replay_seconds < 1, f"{replay_seconds:.2f}s")
    check("sleep replay stdout byte-identical", rep.stdout == rec.stdout == b"slept\n")


def test_exit_code(work, apps):
    tape = work / "exit.tape"
    rec = run([str(apps["exit-app"])], env={"PEN_TAPE": f"record:{tape}"}, cwd=work)
    check("exit-app records exit code 7", rec.returncode == 7, f"exit={rec.returncode}")
    check("exit record has zero stdout", rec.stdout == b"", repr(rec.stdout))
    rep = run([str(apps["exit-app"])], env={"PEN_TAPE": f"replay:{tape}"}, cwd=work)
    check("exit-app replays exit code 7", rep.returncode == 7, f"exit={rep.returncode}")
    check("exit replay has zero stdout", rep.stdout == b"", repr(rep.stdout))


def test_read_twice(work, apps):
    fixture = work / "twice.txt"
    fixture.write_text("hello\n")
    tape = work / "twice.tape"
    rec = run(
        [str(apps["readtwice"]), str(fixture)],
        env={"PEN_TAPE": f"record:{tape}"},
        cwd=work,
    )
    check("readtwice records", rec.returncode == 0, rec.stderr.decode()[:300])
    fixture.unlink()
    rep = run(
        [str(apps["readtwice"]), str(fixture)],
        env={"PEN_TAPE": f"replay:{tape}"},
        cwd=work,
    )
    check("readtwice replays after file deleted", rep.returncode == 0, rep.stderr.decode()[:300])
    check(
        "readtwice replay stdout byte-identical",
        rep.returncode == 0 and rep.stdout == rec.stdout == b"first:hello\n|second:",
        f"record={rec.stdout!r} replay={rep.stdout!r}",
    )


def test_reverse_read_order(work, apps):
    first = work / "order-a.txt"
    second = work / "order-b.txt"
    first.write_text("AAA")
    second.write_text("BBB")
    tape = work / "order.tape"
    rec = run(
        [str(apps["readab"]), str(first), str(second)],
        env={"PEN_TAPE": f"record:{tape}"},
        cwd=work,
    )
    check("readab records", rec.returncode == 0, rec.stderr.decode()[:300])
    first.unlink()
    second.unlink()
    # readba opens and reads the same files in reverse order against the
    # readab tape: per-key FIFO matching still replays.
    rep = run(
        [str(apps["readba"]), str(first), str(second)],
        env={"PEN_TAPE": f"replay:{tape}"},
        cwd=work,
    )
    check(
        "reverse-order reads still replay",
        rep.returncode == 0,
        rep.stderr.decode()[:300],
    )
    check(
        "reverse-order replay stdout byte-identical",
        rep.returncode == 0 and rep.stdout == rec.stdout == b"A:AAA|B:BBB",
        f"record={rec.stdout!r} replay={rep.stdout!r}",
    )


def test_get_time(work, apps):
    tape = work / "time.tape"
    rec = run([str(apps["gettime"])], env={"PEN_TAPE": f"record:{tape}"}, cwd=work)
    check("gettime records", rec.returncode == 0, rec.stderr.decode()[:300])
    try:
        recorded = int(rec.stdout.decode().strip())
    except ValueError:
        recorded = None
    check("gettime prints a millisecond timestamp", recorded is not None, repr(rec.stdout))
    time.sleep(1.2)
    rep = run([str(apps["gettime"])], env={"PEN_TAPE": f"replay:{tape}"}, cwd=work)
    check(
        "gettime replays the exact recorded timestamp, not the wall clock",
        rep.returncode == 0 and rep.stdout == rec.stdout,
        f"record={rec.stdout!r} replay={rep.stdout!r}",
    )


def test_recorded_errors(work, apps):
    fixture = work / "errread.txt"
    fixture.write_text("data")
    tape = work / "errread.tape"
    rec = run(
        [str(apps["errread"]), str(fixture)],
        env={"PEN_TAPE": f"record:{tape}"},
        cwd=work,
    )
    check("errread records", rec.returncode == 0, rec.stderr.decode()[:300])
    check(
        "errread saw real OS errors",
        b"Bad file descriptor" in rec.stdout and b"No such file or directory" in rec.stdout,
        repr(rec.stdout),
    )
    fixture.unlink()
    rep = run(
        [str(apps["errread"]), str(fixture)],
        env={"PEN_TAPE": f"replay:{tape}"},
        cwd=work,
    )
    check(
        "recorded errors replay byte-for-byte",
        rep.returncode == 0 and rep.stdout == rec.stdout,
        f"record={rec.stdout!r} replay={rep.stdout!r}",
    )


def crate_version() -> str:
    for line in (REPO_ROOT / "ffi" / "library" / "Cargo.toml").read_text().splitlines():
        if line.strip().startswith("version"):
            _key, _sep, value = line.partition("=")
            return value.strip().strip('"')
    raise RuntimeError("no version in ffi/library/Cargo.toml")


def rewrite_tape_version(tape: Path, dest: Path, version: str) -> None:
    lines = tape.read_text().splitlines()
    header = json.loads(lines[0])
    header["os_tape"] = version
    lines[0] = json.dumps(header)
    dest.write_text("\n".join(lines) + "\n")


def test_tape_version(work, apps):
    current = crate_version()
    major = current.split(".")[0]
    same_major = f"{major}.99.99"
    other_major = f"{int(major) + 1}.0.0"
    ok_tape = work / "version-ok.tape"
    rewrite_tape_version(work / "sleep.tape", ok_tape, same_major)
    rep = run([str(apps["sleep"])], env={"PEN_TAPE": f"replay:{ok_tape}"}, cwd=work)
    check(
        "same-major tape replays",
        rep.returncode == 0 and rep.stdout == b"slept\n",
        f"exit={rep.returncode} err={rep.stderr.decode()[:300]!r}",
    )
    bad_tape = work / "version-bad.tape"
    rewrite_tape_version(work / "sleep.tape", bad_tape, other_major)
    rep = run([str(apps["sleep"])], env={"PEN_TAPE": f"replay:{bad_tape}"}, cwd=work)
    err = rep.stderr.decode()
    check("other-major tape is refused", rep.returncode != 0, f"exit={rep.returncode}")
    check(
        "refusal names both versions",
        other_major in err and current in err,
        err[:300],
    )


def test_tape_tools(work, apps):
    tape = work / "cat.tape"
    raw = tape.read_bytes()
    lines = [line for line in raw.decode("utf-8").splitlines() if line.strip()]
    header = json.loads(lines[0])
    check("recorded header carries os-tape version", "os_tape" in header, lines[0][:120])
    check("fresh recording uses LF endings (no CR bytes)", b"\r" not in raw)
    try:
        raw.split(b"\n")[0].decode("utf-8")
        header_utf8 = True
    except UnicodeDecodeError:
        header_utf8 = False
    check("recorded header is UTF-8 text", header_utf8)
    proc = run([str(OS_TAPE), "--version"], cwd=work)
    cli_version = proc.stdout.decode().strip().split()[-1] if proc.returncode == 0 else None
    check(
        "os-tape --version matches the recorded header version",
        proc.returncode == 0 and cli_version == header.get("os_tape"),
        f"cli={cli_version!r} header={header.get('os_tape')!r}",
    )
    check(
        "recorded tape ends with a footer",
        json.loads(lines[-1]).get("footer") is True,
        lines[-1][:120],
    )
    proc = run([str(OS_TAPE), "validate", str(tape)], cwd=work)
    check(
        "validate passes on a real recording",
        proc.returncode == 0 and b"valid" in proc.stdout,
        proc.stderr.decode()[:300],
    )
    proc = run([str(OS_TAPE), "info", str(tape)], cwd=work)
    out = proc.stdout.decode()
    check("info runs on a real recording", proc.returncode == 0, proc.stderr.decode()[:300])
    check(
        "info call count matches a manual line count",
        proc.returncode == 0 and f"calls: {len(lines) - 2}" in out,
        out[:300],
    )
    check(
        "info shows per-function counts",
        "read_limit_file:" in out and "write_stdout:" in out,
        out[:300],
    )
    check(
        "info shows a measured duration",
        "duration: " in out and "unknown" not in out,
        out[:300],
    )
    proc = run([str(OS_TAPE), "cat", str(tape), "--seq", "1"], cwd=work)
    try:
        shown = json.loads(proc.stdout.decode())
    except ValueError:
        shown = None
    check(
        "cat shows exactly entry seq 1",
        proc.returncode == 0 and shown is not None and shown.get("seq") == 1,
        proc.stdout.decode()[:200],
    )
    corrupt = work / "corrupt.tape"
    corrupt_lines = tape.read_text().splitlines()
    corrupt_lines[2] = "not json at all"
    corrupt.write_text("\n".join(corrupt_lines) + "\n")
    proc = run([str(OS_TAPE), "validate", str(corrupt)], cwd=work)
    err = proc.stderr.decode()
    check("validate fails on corrupt JSONL", proc.returncode != 0)
    check(
        "validate names the first malformed line",
        "line 3" in err,
        err[:300],
    )


def test_diff_and_check(work, apps):
    tape = work / "cat.tape"
    # Diff mode against the unchanged app: clean.
    clean = run(
        [str(OS_TAPE), "replay", "--diff", str(tape)],
        cwd=work,
    )
    check("diff mode matches unchanged app", clean.returncode == 0, clean.stderr.decode()[:300])
    check(
        "clean diff prints no diff output",
        clean.returncode == 0 and clean.stdout == b"" and clean.stderr == b"",
        f"stdout={clean.stdout!r} stderr={clean.stderr.decode()[:300]!r}",
    )
    # Change one line of output, rebuild, and diff again.
    main = work / "cat" / "main.pen"
    original = main.read_text()
    changed = original.replace(
        'File\'Write(ctx, File\'StdOut(), s)?',
        'File\'Write(ctx, File\'StdOut(), s + "CHANGED\\n")?',
    )
    assert changed != original, "cat patch point moved upstream?"
    main.write_text(changed)
    apps["cat"] = pen_build(work / "cat")
    diffed = run([str(OS_TAPE), "replay", "--diff", str(tape)], cwd=work)
    err = diffed.stderr.decode()
    check("diff mode exits non-zero on change", diffed.returncode != 0)
    check("diff reports exactly the changed line", "+CHANGED" in err and "-CHANGED" not in err, err[:500])
    removed = [line for line in err.splitlines() if line.startswith("-") and not line.startswith("---")]
    added = [line for line in err.splitlines() if line.startswith("+") and not line.startswith("+++")]
    check(
        "diff contains exactly one added line and no removed lines",
        removed == [] and added == ["+CHANGED"],
        err[:500],
    )
    # check/ over a tapes dir fails while the app differs, passes when restored.
    tapes = work / "tapes"
    tapes.mkdir(exist_ok=True)
    for name in ("cat", "ls", "tcp", "udp", "sleep"):
        shutil.copy(work / f"{name}.tape", tapes / f"{name}.tape")
    failing = run([str(OS_TAPE), "check", str(tapes)], cwd=work)
    check("os-tape check fails on changed output", failing.returncode != 0)
    failing_verdicts = [line for line in failing.stdout.decode().splitlines() if line.strip()]
    check(
        "check prints one PASS/FAIL line per tape",
        len(failing_verdicts) == 5
        and sum(line.startswith("PASS ") for line in failing_verdicts) == 4
        and sum(line.startswith("FAIL ") for line in failing_verdicts) == 1,
        repr(failing.stdout.decode()[:300]),
    )
    check(
        "check names the failing tape",
        any(line.startswith("FAIL ") and "cat.tape" in line for line in failing_verdicts),
        repr(failing.stdout.decode()[:300]),
    )
    main.write_text(original)
    apps["cat"] = pen_build(work / "cat")
    passing = run([str(OS_TAPE), "check", str(tapes)], cwd=work)
    check("os-tape check passes on clean tapes", passing.returncode == 0, passing.stderr.decode()[-500:])
    passing_verdicts = [line for line in passing.stdout.decode().splitlines() if line.strip()]
    check(
        "clean check prints all PASS lines",
        len(passing_verdicts) == 5
        and all(line.startswith("PASS ") for line in passing_verdicts),
        repr(passing.stdout.decode()[:300]),
    )
    empty = work / "no-tapes-here"
    empty.mkdir()
    proc = run([str(OS_TAPE), "check", str(empty)], cwd=work)
    check(
        "check on an empty directory exits 0",
        proc.returncode == 0,
        f"exit={proc.returncode}",
    )
    check(
        'empty check prints "no tapes found" to stderr',
        "no tapes found" in proc.stderr.decode() and proc.stdout == b"",
        proc.stderr.decode()[:200],
    )


def test_strict_mismatch(work, apps):
    lines = (work / "cat.tape").read_text().splitlines()
    short = work / "short.tape"
    short.write_text("\n".join(lines[:3]) + "\n")
    proc = run(
        [str(apps["cat"]), "hello.txt"],
        env={"PEN_TAPE": f"replay:{short}"},
        cwd=work,
    )
    err = proc.stderr.decode()
    check("strict replay fails on missing entry", proc.returncode != 0)
    check("mismatch names the call", "replay mismatch" in err and "read_limit_file" in err, err[:300])
    check("mismatch shows the call arguments", "with args" in err and '"h"' in err, err[:300])
    check(
        "mismatch points at the nearest tape entries",
        "nearest entries" in err or "closest remaining entries" in err,
        err[:300],
    )
    check(
        "mismatch names the PEN_TAPE mode and tape path",
        f"PEN_TAPE=replay:{short}" in err,
        err[:300],
    )
    report_lines = err.strip().splitlines()
    check(
        "mismatch output fits in 40 lines",
        len(report_lines) <= 40,
        f"{len(report_lines)} lines",
    )
    check(
        "mismatch ends with a re-record suggestion",
        bool(report_lines) and "re-record with os-tape record" in report_lines[-1],
        report_lines[-1][:200] if report_lines else "<empty>",
    )


def test_diff_ignores_sleep_timing(work, apps):
    fast_src = SLEEP_MAIN.replace("10000", "100")
    assert fast_src != SLEEP_MAIN
    fast = build_extra_package(work, "sleep-fast", fast_src, {"Console": "../console"})
    tape = work / "sleep.tape"
    diffed = run(
        [str(OS_TAPE), "replay", "--diff", str(tape), "--", str(fast)],
        cwd=work,
    )
    check(
        "diff mode ignores a changed Sleep duration",
        diffed.returncode == 0,
        diffed.stderr.decode()[:300],
    )
    strict = run(
        [str(fast)],
        env={"PEN_TAPE": f"replay:{tape}"},
        cwd=work,
    )
    check(
        "strict replay still rejects a changed Sleep duration",
        strict.returncode != 0 and "replay mismatch" in strict.stderr.decode(),
        strict.stderr.decode()[:300],
    )


def test_diff_file_write_and_extra_output(work, apps):
    assert WRITEOUT_V1 != WRITEOUT_V2 != WRITEOUT_V3, "writeout variants must differ"
    package = work / "writeout"
    app = build_extra_package(
        work, "writeout", WRITEOUT_V1, {"Console": "../console"}
    )
    target = work / "diff-target.txt"
    if target.exists():
        target.unlink()
    tape = work / "writeout.tape"
    rec = run(
        [str(app), str(target)],
        env={"PEN_TAPE": f"record:{tape}"},
        cwd=work,
    )
    check("writeout records", rec.returncode == 0, rec.stderr.decode()[:300])
    written = target.read_text() if target.exists() else "<missing>"
    check("writeout wrote one line", written == "line1\n", repr(written))
    # Same path, changed contents: the diff names the file and the new line.
    (package / "main.pen").write_text(WRITEOUT_V2)
    apps["writeout"] = pen_build(package)
    diffed = run([str(OS_TAPE), "replay", "--diff", str(tape)], cwd=work)
    err = diffed.stderr.decode()
    check("file-write change fails the diff", diffed.returncode != 0)
    check("file-write diff names the file path", str(target) in err, err[:500])
    check(
        "file-write diff shows a unified diff of the new line",
        "@@" in err and "+line2" in err and "-line2" not in err,
        err[:500],
    )
    # Same path, one more write: the extra output names the unexpected call.
    (package / "main.pen").write_text(WRITEOUT_V3)
    apps["writeout"] = pen_build(package)
    diffed = run([str(OS_TAPE), "replay", "--diff", str(tape)], cwd=work)
    err = diffed.stderr.decode()
    check("extra output fails the diff", diffed.returncode != 0)
    check(
        "extra output names the unexpected call",
        "extra output" in err and "write_file" in err,
        err[:500],
    )


def test_size_cap(work, apps):
    tape = work / "cap.tape"
    proc = run(
        [str(apps["exit-app"])],
        env={"PEN_TAPE": f"record:{tape}", "PEN_TAPE_MAX_BYTES": "frobnicate"},
        cwd=work,
    )
    err = proc.stderr.decode()
    check(
        "invalid PEN_TAPE_MAX_BYTES fails fast",
        proc.returncode == 1,
        f"exit={proc.returncode}",
    )
    check(
        "invalid cap is a parse error naming the variable",
        "invalid" in err and "PEN_TAPE_MAX_BYTES" in err and "frobnicate" in err,
        err[:300],
    )
    check(
        "cap parse error names the PEN_TAPE mode and tape path",
        "PEN_TAPE=record:" in err and str(tape) in err,
        err[:300],
    )
    fixture = work / "cap-input.txt"
    fixture.write_text("hello cap\n")
    proc = run(
        [str(apps["cat"]), str(fixture)],
        env={"PEN_TAPE": f"record:{tape}", "PEN_TAPE_MAX_BYTES": "1K"},
        cwd=work,
    )
    err = proc.stderr.decode()
    check("tiny cap aborts the recording", proc.returncode != 0, f"exit={proc.returncode}")
    check(
        "cap error names the cap and the variable",
        "size cap" in err and "PEN_TAPE_MAX_BYTES" in err and "1024" in err,
        err[:300],
    )
    check(
        "cap error names the PEN_TAPE mode and tape path",
        "PEN_TAPE=record:" in err and str(tape) in err,
        err[:300],
    )
    big = work / "big.bin"
    big.write_bytes(b"x" * (2 * 1024 * 1024))
    proc = run(
        [str(apps["cat"]), str(big)],
        env={"PEN_TAPE": f"record:{tape}", "PEN_TAPE_MAX_BYTES": "1M"},
        cwd=work,
    )
    err = proc.stderr.decode()
    check("1M cap enforced on a 2 MiB recording", proc.returncode != 0, f"exit={proc.returncode}")
    check(
        "1M parses to 1048576 bytes",
        "1048576" in err and "PEN_TAPE_MAX_BYTES" in err,
        err[:300],
    )
    proc = run(
        [str(apps["cat"]), str(big)],
        env={"PEN_TAPE": f"record:{tape}"},
        cwd=work,
    )
    check(
        "same recording fits the default 512 MiB cap",
        proc.returncode == 0,
        proc.stderr.decode()[:300],
    )


def test_blob_spill(work, apps):
    payload = bytes(range(256)) * 4096  # exactly 1 MiB
    assert len(payload) == 1024 * 1024
    fixture = work / "spill.bin"
    fixture.write_bytes(payload)
    tape = work / "spill.tape"
    rec = run(
        [str(apps["readtwice"]), str(fixture)],
        env={"PEN_TAPE": f"record:{tape}"},
        cwd=work,
    )
    check("1 MiB read records", rec.returncode == 0, rec.stderr.decode()[:300])
    digest = hashlib.sha256(payload).hexdigest()
    entries = [
        json.loads(line)
        for line in tape.read_text().splitlines()
        if line.strip()
    ][1:]
    spilled = [
        entry
        for entry in entries
        if isinstance(entry.get("ok"), dict) and entry["ok"].get("blob") == digest
    ]
    check(
        "tape line references blobs/<sha256>",
        len(spilled) >= 1,
        f"{len(entries)} entries, digest {digest[:16]}...",
    )
    blob_file = work / "blobs" / digest
    check(
        "blob file bytes match the recorded read",
        blob_file.is_file() and blob_file.read_bytes() == payload,
    )
    fixture.unlink()
    rep = run(
        [str(apps["readtwice"]), str(fixture)],
        env={"PEN_TAPE": f"replay:{tape}"},
        cwd=work,
    )
    check(
        "blob-backed replay succeeds with byte-identical stdout",
        rep.returncode == 0 and rep.stdout == rec.stdout,
        f"exit={rep.returncode} err={rep.stderr.decode()[:200]!r}",
    )


def test_evil_blob(work, apps):
    fixture = work / "evil-input.txt"
    fixture.write_text("hello\n")
    clean = work / "evil-clean.tape"
    rec = run(
        [str(apps["readtwice"]), str(fixture)],
        env={"PEN_TAPE": f"record:{clean}"},
        cwd=work,
    )
    check("evil-blob source records", rec.returncode == 0, rec.stderr.decode()[:300])
    sentinel = work / "evil"
    sentinel.write_bytes(b"sensitive-bytes")
    blobs = work / "blobs"
    before = sorted(path.name for path in blobs.iterdir()) if blobs.is_dir() else []
    lines = clean.read_text().splitlines()
    patched = False
    out = [lines[0]]
    for line in lines[1:]:
        entry = json.loads(line)
        if (
            not patched
            and entry.get("fn") in ("read_file", "read_limit_file")
            and "ok" in entry
        ):
            entry["ok"] = {"blob": "../evil"}
            patched = True
        out.append(json.dumps(entry))
    evil = work / "evil.tape"
    evil.write_text("\n".join(out) + "\n")
    check("evil tape patches a byte-string result", patched)
    proc = run(
        [str(apps["readtwice"]), str(fixture)],
        env={"PEN_TAPE": f"replay:{evil}"},
        cwd=work,
    )
    err = proc.stderr.decode()
    check("traversal blob is rejected", proc.returncode != 0, f"exit={proc.returncode}")
    check(
        "rejection names the bad digest",
        "bad blob digest" in err and "../evil" in err,
        err[:300],
    )
    check("sentinel outside blobs/ untouched", sentinel.read_bytes() == b"sensitive-bytes")
    after = sorted(path.name for path in blobs.iterdir()) if blobs.is_dir() else []
    check("replay writes no blob files", after == before)
    validate = run([str(OS_TAPE), "validate", str(evil)], cwd=work)
    check(
        "os-tape validate rejects the traversal digest",
        validate.returncode != 0 and "bad blob digest" in validate.stderr.decode(),
        validate.stderr.decode()[:300],
    )


def test_many_lines(work, apps):
    app = build_extra_package(work, "manylines", MANYLINES_MAIN, {"Console": "../console"})
    tape = work / "manylines.tape"
    start = time.monotonic()
    rec = run([str(app)], env={"PEN_TAPE": f"record:{tape}"}, cwd=work, timeout=120)
    elapsed = time.monotonic() - start
    check("10,000-line program records", rec.returncode == 0, rec.stderr.decode()[:300])
    check(
        "recording finishes in under 30 s",
        rec.returncode == 0 and elapsed < 30,
        f"{elapsed:.1f}s",
    )
    size = tape.stat().st_size if tape.exists() else None
    check(
        "tape stays under 5 MiB",
        size is not None and size < 5 * 1024 * 1024,
        f"{size} bytes",
    )
    check(
        "program wrote 10,000 lines",
        rec.stdout.count(b"\n") == 10000,
        f"{rec.stdout.count(bytes([10]))} newlines",
    )
    rep = run([str(app)], env={"PEN_TAPE": f"replay:{tape}"}, cwd=work, timeout=120)
    check(
        "10,000-line replay stdout byte-identical",
        rep.returncode == 0 and rep.stdout == rec.stdout,
        f"exit={rep.returncode} record={len(rec.stdout)} replay={len(rep.stdout)}",
    )


def test_startup_failures(work, apps):
    proc = run([str(apps["exit-app"])], env={"PEN_TAPE": "frobnicate"}, cwd=work)
    err = proc.stderr.decode()
    check(
        "malformed PEN_TAPE fails instead of running live",
        proc.returncode == 1,
        f"exit={proc.returncode}",
    )
    check(
        "malformed PEN_TAPE error lists the valid forms",
        "record:<file>" in err and "replay:<file>" in err,
        err[:300],
    )
    check("malformed PEN_TAPE runs nothing", proc.stdout == b"", repr(proc.stdout))
    check(
        "malformed PEN_TAPE error names the rejected value",
        "PEN_TAPE=frobnicate" in err,
        err[:300],
    )
    blocker = work / "blocker"
    blocker.write_text("not a directory")
    bad = blocker / "x.tape"
    proc = run(
        [str(apps["exit-app"])],
        env={"PEN_TAPE": f"record:{bad}"},
        cwd=work,
    )
    err = proc.stderr.decode()
    check(
        "unwritable tape path fails at startup",
        proc.returncode == 1 and proc.stdout == b"",
        f"exit={proc.returncode} stdout={proc.stdout!r}",
    )
    check(
        "unwritable tape error names the path and OS error",
        "cannot create" in err and "blocker" in err and "Not a directory" in err,
        err[:300],
    )
    check(
        "unwritable tape error names the PEN_TAPE mode",
        f"PEN_TAPE=record:{bad}" in err,
        err[:300],
    )
    adir = work / "a-directory"
    adir.mkdir()
    proc = run(
        [str(apps["exit-app"])],
        env={"PEN_TAPE": f"record:{adir}"},
        cwd=work,
    )
    err = proc.stderr.decode()
    check(
        "directory as tape path fails at startup",
        proc.returncode == 1 and proc.stdout == b"",
        f"exit={proc.returncode} stdout={proc.stdout!r}",
    )
    check(
        "directory tape error names the path and OS error",
        "cannot create tape" in err and "a-directory" in err and "Is a directory" in err,
        err[:300],
    )
    check(
        "directory tape error names the PEN_TAPE mode",
        f"PEN_TAPE=record:{adir}" in err,
        err[:300],
    )
    missing = work / "missing.tape"
    proc = run(
        [str(apps["exit-app"])],
        env={"PEN_TAPE": f"replay:{missing}"},
        cwd=work,
    )
    err = proc.stderr.decode()
    check("missing replay tape fails", proc.returncode != 0, f"exit={proc.returncode}")
    check(
        "missing tape error names the path",
        str(missing) in err and "No such file" in err,
        err[:300],
    )
    check(
        "missing tape error names the PEN_TAPE mode",
        f"PEN_TAPE=replay:{missing}" in err,
        err[:300],
    )
    check(
        "missing tape fails without a panic backtrace",
        "panicked" not in err and "backtrace" not in err.lower(),
        err[:300],
    )


def test_concurrent_recorders(work, apps):
    tape = work / "race.tape"
    if tape.exists():
        tape.unlink()
    env = dict(os.environ)
    env["PEN_TAPE"] = f"record:{tape}"
    first = subprocess.Popen(
        [str(apps["sleep"])],
        env=env,
        cwd=work,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    try:
        time.sleep(1.0)
        check("first recorder still running when the second starts", first.poll() is None)
        start = time.monotonic()
        second = run([str(apps["sleep"])], env={"PEN_TAPE": f"record:{tape}"}, cwd=work)
        elapsed = time.monotonic() - start
        err = second.stderr.decode()
        check(
            "second recorder fails instead of interleaving",
            second.returncode != 0,
            f"exit={second.returncode}",
        )
        check('second recorder reports "tape locked"', "tape locked" in err, err[:300])
        check("second recorder fails fast", elapsed < 5, f"{elapsed:.1f}s")
    finally:
        first.terminate()
        try:
            first.wait(timeout=30)
        except subprocess.TimeoutExpired:
            first.kill()
            first.wait(timeout=30)


def test_sigint_flushes_partial_tape(work, apps):
    app = build_extra_package(work, "looper", LOOPER_MAIN, {"Console": "../console"})
    args = [f"line-{index}" for index in range(60)]
    tape = work / "looper.tape"
    env = dict(os.environ)
    env["PEN_TAPE"] = f"record:{tape}"
    proc = subprocess.Popen(
        [str(app), *args],
        env=env,
        cwd=work,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    time.sleep(2.0)
    proc.send_signal(signal.SIGINT)
    try:
        proc.wait(timeout=30)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=30)
        check("interrupted recording stops on SIGINT", False, "timed out waiting")
        return
    check("interrupted recording exits non-zero", proc.returncode != 0, f"exit={proc.returncode}")
    valid = run([str(OS_TAPE), "validate", str(tape)], cwd=work)
    check(
        "partial tape validates up to the last full line",
        valid.returncode == 0,
        valid.stderr.decode()[:300],
    )
    info = run([str(OS_TAPE), "info", str(tape)], cwd=work)
    out = info.stdout.decode()
    calls = None
    for line in out.splitlines():
        if line.startswith("calls:"):
            try:
                calls = int(line.split(":")[1])
            except ValueError:
                calls = None
    check(
        "partial tape holds complete flushed entries",
        info.returncode == 0 and calls is not None and calls > 5,
        out[:300],
    )
    check(
        "partial tape has no footer duration",
        "unknown" in out,
        out[:300],
    )


def main() -> int:
    print(f"repo: {REPO_ROOT}")
    print(f"PEN_ROOT: {os.environ.get('PEN_ROOT')}")
    work = Path(tempfile.mkdtemp(prefix="os-tape-e2e-"))
    print(f"work: {work}")
    try:
        staged = stage_examples(work)
        apps = {}
        for name in (
            "cat",
            "ls",
            "tcp-client",
            "udp-client",
            "sleep",
            "exit-app",
            "readtwice",
            "readab",
            "readba",
            "gettime",
            "errread",
        ):
            print(f"building {name} ...")
            apps[name] = pen_build(staged[name])
        test_cat(work, apps)
        test_ls(work, apps)
        test_tcp_client(work, apps)
        test_udp_client(work, apps)
        test_sleep(work, apps)
        test_exit_code(work, apps)
        test_read_twice(work, apps)
        test_reverse_read_order(work, apps)
        test_get_time(work, apps)
        test_recorded_errors(work, apps)
        test_tape_version(work, apps)
        test_tape_tools(work, apps)
        test_diff_and_check(work, apps)
        test_strict_mismatch(work, apps)
        test_diff_ignores_sleep_timing(work, apps)
        test_diff_file_write_and_extra_output(work, apps)
        test_size_cap(work, apps)
        test_blob_spill(work, apps)
        test_evil_blob(work, apps)
        test_many_lines(work, apps)
        test_startup_failures(work, apps)
        test_concurrent_recorders(work, apps)
        test_sigint_flushes_partial_tape(work, apps)
    finally:
        shutil.rmtree(work, ignore_errors=True)
    print()
    if FAILURES:
        print(f"{len(FAILURES)} FAILURES: {FAILURES}")
        return 1
    print("all e2e checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
