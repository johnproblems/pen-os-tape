#!/usr/bin/env python3
"""Unit tests for bin/os-tape. Stub-based; needs no Pen toolchain.

Run from the repo root:  python3 -m unittest discover -s tests -v
"""

import json
import os
import shutil
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
OS_TAPE = REPO_ROOT / "bin" / "os-tape"


def write_stub(path: Path, body: str) -> None:
    path.write_text(body)
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


APP_STUB = """#!/bin/sh
echo "PEN_TAPE=$PEN_TAPE" >> "$OS_TAPE_TEST_LOG"
echo "PEN_TAPE_OUTPUTS=$PEN_TAPE_OUTPUTS" >> "$OS_TAPE_TEST_LOG"
echo "ARGS=$*" >> "$OS_TAPE_TEST_LOG"
exit ${OS_TAPE_TEST_EXIT:-0}
"""

PEN_STUB = """#!/bin/sh
echo "pen $*" >> "$OS_TAPE_TEST_LOG"
exit ${OS_TAPE_TEST_PEN_EXIT:-0}
"""


def write_tape(path: Path, argv, entries=()) -> None:
    lines = [
        json.dumps({"header": True, "v": 1, "os_tape": "0.1.0", "argv": argv, "env": {}})
    ]
    for index, (func, args, ok) in enumerate(entries, start=1):
        lines.append(json.dumps({"seq": index, "fn": func, "args": args, "ok": ok}))
    path.write_text("\n".join(lines) + "\n")


class OsTapeTestCase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="os-tape-test-")
        self.dir = Path(self.tmp.name)
        self.log = self.dir / "calls.log"
        self.env = dict(os.environ)
        self.env["OS_TAPE_TEST_LOG"] = str(self.log)
        self.env["PATH"] = str(self.dir) + os.pathsep + os.environ["PATH"]
        write_stub(self.dir / "pen", PEN_STUB)
        self.app = self.dir / "app"
        write_stub(self.app, APP_STUB)
        (self.dir / "pen.json").write_text(
            json.dumps({"type": "application", "dependencies": {"Os": "pen:///os-tape"}})
        )

    def tearDown(self):
        self.tmp.cleanup()

    def run_os_tape(self, *args, cwd=None):
        return subprocess.run(
            [str(OS_TAPE), *args],
            env=self.env,
            cwd=cwd or self.dir,
            capture_output=True,
            text=True,
        )

    def logged(self):
        if not self.log.exists():
            return []
        return self.log.read_text().splitlines()


class RecordTests(OsTapeTestCase):
    def test_record_builds_then_runs_with_record_env(self):
        proc = self.run_os_tape("record", "-o", "run.tape", "--", str(self.app), "a", "b")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        lines = self.logged()
        self.assertTrue(lines[0].startswith("pen build"), lines)
        self.assertIn("PEN_TAPE=record:run.tape", lines)
        self.assertIn("PEN_TAPE_OUTPUTS=", lines)
        self.assertIn(f"ARGS=a b", lines)

    def test_record_defaults_tape_under_tapes_dir(self):
        proc = self.run_os_tape("record", "--", str(self.app))
        self.assertEqual(proc.returncode, 0, proc.stderr)
        tapes = list(self.dir.glob("tapes/app-*.tape"))
        # The tape file itself is created by the app, not the CLI; the CLI
        # only ensures the directory and passes the path through.
        self.assertTrue((self.dir / "tapes").is_dir())
        self.assertEqual(tapes, [])
        lines = self.logged()
        self.assertTrue(
            any(line.startswith("PEN_TAPE=record:tapes/app-") for line in lines), lines
        )

    def test_record_propagates_app_exit_code(self):
        self.env["OS_TAPE_TEST_EXIT"] = "3"
        proc = self.run_os_tape("record", "-o", "x.tape", "--", str(self.app))
        self.assertEqual(proc.returncode, 3)

    def test_record_fails_when_build_fails(self):
        self.env["OS_TAPE_TEST_PEN_EXIT"] = "1"
        proc = self.run_os_tape("record", "-o", "x.tape", "--", str(self.app))
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("pen build failed", proc.stderr)
        self.assertNotIn("PEN_TAPE=", "\n".join(self.logged()))

    def test_record_warns_on_uncovered_dependencies(self):
        (self.dir / "pen.json").write_text(
            json.dumps(
                {
                    "type": "application",
                    "dependencies": {
                        "Os": "pen:///os-tape",
                        "Http": "pen:///http",
                        "Sql": "pen:///sql",
                        "Random": "pen:///random",
                    },
                }
            )
        )
        proc = self.run_os_tape("record", "-o", "x.tape", "--", str(self.app))
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("does not cover", proc.stderr)
        for name in ("Http", "Sql", "Random"):
            self.assertIn(name, proc.stderr)

    def test_record_needs_command(self):
        proc = self.run_os_tape("record", "-o", "x.tape")
        self.assertEqual(proc.returncode, 2)
        self.assertIn("needs an app command", proc.stderr)
        self.assertIn("--", proc.stderr)

    def test_record_without_separator_shows_double_dash(self):
        proc = self.run_os_tape("record", str(self.app), "a", "b")
        self.assertEqual(proc.returncode, 2, proc.stderr)
        self.assertIn("--", proc.stderr)

    def test_record_tape_option_selects_output(self):
        proc = self.run_os_tape("record", "--tape", "out.tape", "--", str(self.app))
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("PEN_TAPE=record:out.tape", self.logged())


class ReplayTests(OsTapeTestCase):
    def test_replay_uses_recorded_argv(self):
        tape = self.dir / "run.tape"
        write_tape(tape, [str(self.app), "hello"])
        proc = self.run_os_tape("replay", str(tape))
        self.assertEqual(proc.returncode, 0, proc.stderr)
        lines = self.logged()
        self.assertIn(f"PEN_TAPE=replay:{tape}", lines)
        self.assertIn("ARGS=hello", lines)

    def test_replay_command_overrides_recorded_argv(self):
        tape = self.dir / "run.tape"
        write_tape(tape, [str(self.app), "hello"])
        proc = self.run_os_tape("replay", str(tape), "--", str(self.app), "other")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("ARGS=other", self.logged())

    def test_replay_diff_sets_outputs_mode(self):
        tape = self.dir / "run.tape"
        write_tape(tape, [str(self.app)])
        proc = self.run_os_tape("replay", "--diff", str(tape))
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("PEN_TAPE_OUTPUTS=diff", self.logged())

    def test_replay_rejects_non_tape(self):
        bad = self.dir / "bad.tape"
        bad.write_text("not json\n")
        proc = self.run_os_tape("replay", str(bad))
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("not a tape header", proc.stderr)
        self.assertIn("PEN_TAPE", proc.stderr)
        self.assertIn(str(bad), proc.stderr)

    def test_replay_missing_tape_names_mode_and_path(self):
        missing = self.dir / "missing.tape"
        proc = self.run_os_tape("replay", str(missing))
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn(f"PEN_TAPE=replay:{missing}", proc.stderr)

    def test_replay_without_tape_prints_usage(self):
        proc = self.run_os_tape("replay")
        self.assertEqual(proc.returncode, 2)
        self.assertIn("usage:", proc.stderr)
        self.assertEqual(proc.stdout, "")


class CheckTests(OsTapeTestCase):
    def write_case(self, name, exit_code):
        tapes = self.dir / "tapes"
        tapes.mkdir(exist_ok=True)
        app = self.dir / f"app-{name}"
        body = APP_STUB.replace("OS_TAPE_TEST_EXIT:-0", f"OS_TAPE_TEST_EXIT:-{exit_code}")
        write_stub(app, body)
        write_tape(tapes / f"{name}.tape", [str(app)])
        return tapes / f"{name}.tape"

    def test_check_passes_when_all_match(self):
        self.write_case("a", 0)
        self.write_case("b", 0)
        proc = self.run_os_tape("check", "tapes")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("all 2 tapes match", proc.stderr)
        self.assertEqual(
            sum(1 for line in self.logged() if line == "PEN_TAPE_OUTPUTS=diff"), 2
        )

    def test_check_fails_when_any_differ(self):
        self.write_case("a", 0)
        self.write_case("b", 1)
        proc = self.run_os_tape("check", "tapes")
        self.assertEqual(proc.returncode, 1)
        self.assertIn("1 of 2 tapes differ", proc.stderr)

    def test_check_prints_one_pass_fail_line_per_tape(self):
        self.write_case("a", 0)
        self.write_case("b", 1)
        proc = self.run_os_tape("check", "tapes")
        self.assertEqual(proc.returncode, 1)
        verdicts = [line for line in proc.stdout.splitlines() if line.strip()]
        self.assertEqual(len(verdicts), 2, proc.stdout)
        self.assertTrue(
            any(line.startswith("PASS ") and "a.tape" in line for line in verdicts),
            proc.stdout,
        )
        self.assertTrue(
            any(line.startswith("FAIL ") and "b.tape" in line for line in verdicts),
            proc.stdout,
        )

    def test_check_pass_prints_pass_lines(self):
        self.write_case("a", 0)
        proc = self.run_os_tape("check", "tapes")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        verdicts = [line for line in proc.stdout.splitlines() if line.strip()]
        self.assertEqual(len(verdicts), 1, proc.stdout)
        self.assertIn("a.tape", verdicts[0])
        self.assertTrue(verdicts[0].startswith("PASS "), proc.stdout)

    def test_check_empty_directory_succeeds(self):
        (self.dir / "empty").mkdir()
        proc = self.run_os_tape("check", "empty")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("no tapes found", proc.stderr)

    def test_check_rejects_non_directory(self):
        proc = self.run_os_tape("check", "missing-dir")
        self.assertEqual(proc.returncode, 2)
        self.assertIn("not a directory", proc.stderr)


class HelpTests(OsTapeTestCase):
    def test_help_and_version(self):
        for args in (["--help"], ["-h"], []):
            proc = self.run_os_tape(*args)
            self.assertEqual(proc.returncode, 0)
            self.assertIn("Usage:", proc.stdout)
        proc = self.run_os_tape("version")
        self.assertEqual(proc.returncode, 0)
        self.assertIn("os-tape 0.1.0", proc.stdout)

    def test_unknown_command(self):
        proc = self.run_os_tape("frobnicate")
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("unknown command", proc.stderr)

    def test_help_lists_all_subcommands(self):
        proc = self.run_os_tape("--help")
        self.assertEqual(proc.returncode, 0)
        for command in ("record", "replay", "check", "validate", "info", "cat"):
            self.assertIn(command, proc.stdout)

    def test_help_covers_every_readme_command_and_flag(self):
        readme = (REPO_ROOT / "README.md").read_text()
        block = readme.split("```sh")[1].split("```")[0]
        wanted_subcommands = set()
        wanted_flags = set()
        for line in block.splitlines():
            line = line.split("#")[0].strip()
            if not line.startswith("os-tape "):
                continue
            tokens = [token.strip("[],") for token in line.split()]
            if len(tokens) > 1 and not tokens[1].startswith("-"):
                wanted_subcommands.add(tokens[1])
            for token in tokens[2:]:
                if token.startswith("--"):
                    wanted_flags.add(token)
                elif token.startswith("-") and token != "-" and token != "--":
                    wanted_flags.add(token)
        self.assertTrue(wanted_subcommands, "no os-tape commands found in README")
        self.assertTrue(wanted_flags, "no os-tape flags found in README")
        proc = self.run_os_tape("--help")
        self.assertEqual(proc.returncode, 0)
        for command in wanted_subcommands:
            self.assertIn(command, proc.stdout)
        for flag in wanted_flags:
            self.assertIn(flag, proc.stdout)

    def test_version_matches_cargo_toml(self):
        cargo = (REPO_ROOT / "ffi" / "library" / "Cargo.toml").read_text()
        pinned = None
        for line in cargo.splitlines():
            if line.strip().startswith("version"):
                _, _, value = line.partition("=")
                pinned = value.strip().strip('"')
                break
        self.assertIsNotNone(pinned)
        proc = self.run_os_tape("--version")
        self.assertEqual(proc.returncode, 0)
        self.assertEqual(proc.stdout.strip(), f"os-tape {pinned}")


def write_full_tape(path, argv, entries, header_extra=None, footer=None):
    """Write a tape with explicit entry dicts plus an optional footer line."""
    header = {"header": True, "v": 1, "os_tape": "0.1.0", "argv": argv, "env": {}}
    header.update(header_extra or {})
    lines = [json.dumps(header)]
    for entry in entries:
        lines.append(json.dumps(entry))
    if footer is not None:
        lines.append(json.dumps(footer))
    path.write_text("\n".join(lines) + "\n")


def entry(seq, func, args, ok=None, err=None):
    obj = {"seq": seq, "fn": func, "args": args}
    if err is not None:
        obj["err"] = err
    else:
        obj["ok"] = ok
    return obj


class ValidateTests(OsTapeTestCase):
    def test_validate_passes_on_well_formed_tape(self):
        tape = self.dir / "good.tape"
        write_full_tape(
            tape,
            ["app"],
            [
                entry(1, "get_arguments", [], ok=[{"t": "app"}]),
                entry(2, "write_stdout", [{"t": "hi\n"}], ok=3.0),
                entry(3, "open_file", [{"t": "nope"}], err="missing"),
            ],
            footer={"footer": True, "end_time": 2.0, "calls": 3},
        )
        proc = self.run_os_tape("validate", str(tape))
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("valid (3 calls)", proc.stdout)

    def test_validate_names_first_malformed_line(self):
        tape = self.dir / "bad.tape"
        write_full_tape(tape, ["app"], [entry(1, "get_time", [], ok=1.0)])
        with tape.open("a") as handle:
            handle.write("this is not json\n")
            handle.write("{also bad\n")
        proc = self.run_os_tape("validate", str(tape))
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("line 3", proc.stderr)
        self.assertNotIn("line 4", proc.stderr)

    def test_validate_rejects_bad_header(self):
        tape = self.dir / "bad.tape"
        tape.write_text("not json\n")
        proc = self.run_os_tape("validate", str(tape))
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("line 1", proc.stderr)

    def test_validate_rejects_unknown_function(self):
        tape = self.dir / "bad.tape"
        write_full_tape(tape, ["app"], [entry(1, "frobnicate", [], ok=None)])
        proc = self.run_os_tape("validate", str(tape))
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("line 2", proc.stderr)
        self.assertIn("frobnicate", proc.stderr)

    def test_validate_rejects_entry_missing_ok_and_err(self):
        tape = self.dir / "bad.tape"
        write_full_tape(tape, ["app"], [{"seq": 1, "fn": "get_time", "args": []}])
        proc = self.run_os_tape("validate", str(tape))
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("line 2", proc.stderr)
        self.assertIn("ok/err", proc.stderr)

    def test_validate_fails_on_missing_blob_hash(self):
        digest = "e" * 64
        tape = self.dir / "blobs.tape"
        write_full_tape(
            tape, ["app"], [entry(1, "read_file", [{"h": 1}], ok={"blob": digest})]
        )
        proc = self.run_os_tape("validate", str(tape))
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn(digest, proc.stderr)
        self.assertIn("missing blob", proc.stderr)

    def test_validate_passes_when_blob_present(self):
        import hashlib

        data = b"big value here"
        digest = hashlib.sha256(data).hexdigest()
        blobs = self.dir / "blobs"
        blobs.mkdir()
        (blobs / digest).write_bytes(data)
        tape = self.dir / "blobs.tape"
        write_full_tape(
            tape, ["app"], [entry(1, "read_file", [{"h": 1}], ok={"blob": digest})]
        )
        proc = self.run_os_tape("validate", str(tape))
        self.assertEqual(proc.returncode, 0, proc.stderr)

    def test_validate_rejects_bad_blob_digest(self):
        tape = self.dir / "evil.tape"
        write_full_tape(
            tape, ["app"], [entry(1, "read_file", [{"h": 1}], ok={"blob": "../evil"})]
        )
        proc = self.run_os_tape("validate", str(tape))
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("bad blob digest", proc.stderr)

    def test_validate_passes_on_interrupted_tape_without_footer(self):
        tape = self.dir / "partial.tape"
        write_full_tape(
            tape,
            ["app"],
            [
                entry(1, "get_arguments", [], ok=[{"t": "app"}]),
                entry(2, "write_stdout", [{"t": "half\n"}], ok=5.0),
            ],
        )
        proc = self.run_os_tape("validate", str(tape))
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("valid (2 calls)", proc.stdout)


class InfoTests(OsTapeTestCase):
    def write_sample(self):
        import hashlib

        payload = b"x" * 100
        digest = hashlib.sha256(payload).hexdigest()
        blobs = self.dir / "blobs"
        blobs.mkdir()
        (blobs / digest).write_bytes(payload)
        tape = self.dir / "run.tape"
        write_full_tape(
            tape,
            ["app", "a"],
            [
                entry(1, "get_arguments", [], ok=[{"t": "app"}, {"t": "a"}]),
                entry(2, "open_file", [{"t": "a"}], ok={"h": 1}),
                entry(3, "read_file", [{"h": 1}], ok={"blob": digest}),
                entry(4, "write_stdout", [{"t": "x"}], ok=1.0),
                entry(5, "write_stdout", [{"t": "y"}], ok=1.0),
            ],
            header_extra={"start_time": 1000.0},
            footer={"footer": True, "end_time": 2500.0, "calls": 5},
        )
        return tape, digest

    def test_info_matches_manual_counts(self):
        tape, _digest = self.write_sample()
        proc = self.run_os_tape("info", str(tape))
        self.assertEqual(proc.returncode, 0, proc.stderr)
        # Manual recount from the file: 1 header + 5 entries + 1 footer.
        lines = [line for line in tape.read_text().splitlines() if line.strip()]
        self.assertEqual(len(lines), 7)
        self.assertIn("calls: 5", proc.stdout)
        self.assertIn("get_arguments: 1", proc.stdout)
        self.assertIn("open_file: 1", proc.stdout)
        self.assertIn("read_file: 1", proc.stdout)
        self.assertIn("write_stdout: 2", proc.stdout)
        self.assertIn("duration: 1.500s", proc.stdout)
        blob_bytes = sum(
            path.stat().st_size for path in (self.dir / "blobs").iterdir()
        )
        self.assertIn(f"blobs: 1 files, {blob_bytes} bytes", proc.stdout)

    def test_info_without_footer_reports_unknown_duration(self):
        tape = self.dir / "run.tape"
        write_full_tape(tape, ["app"], [entry(1, "get_time", [], ok=1.0)])
        proc = self.run_os_tape("info", str(tape))
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("calls: 1", proc.stdout)
        self.assertIn("duration: unknown", proc.stdout)
        self.assertIn("blobs: 0 files, 0 bytes", proc.stdout)

    def test_info_fails_on_malformed_tape(self):
        tape = self.dir / "bad.tape"
        tape.write_text("nope\n")
        proc = self.run_os_tape("info", str(tape))
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("line 1", proc.stderr)


class CatTests(OsTapeTestCase):
    def test_cat_prints_exactly_the_requested_entry(self):
        tape = self.dir / "run.tape"
        write_full_tape(
            tape,
            ["app"],
            [
                entry(40, "get_time", [], ok=111.0),
                entry(41, "read_file", [{"h": 3}], ok={"t": "hello\n"}),
                entry(42, "write_stdout", [{"t": "hello\n"}], ok=6.0),
            ],
        )
        proc = self.run_os_tape("cat", str(tape), "--seq", "41")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(
            json.loads(proc.stdout),
            {"seq": 41, "fn": "read_file", "args": [{"h": 3}], "ok": {"t": "hello\n"}},
        )

    def test_cat_resolves_blob_references_inline(self):
        import hashlib

        payload = "inline me".encode()
        digest = hashlib.sha256(payload).hexdigest()
        blobs = self.dir / "blobs"
        blobs.mkdir()
        (blobs / digest).write_bytes(payload)
        tape = self.dir / "run.tape"
        write_full_tape(
            tape,
            ["app"],
            [entry(7, "read_file", [{"h": 1}], ok={"blob": digest})],
        )
        proc = self.run_os_tape("cat", str(tape), "--seq=7")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(
            json.loads(proc.stdout),
            {"seq": 7, "fn": "read_file", "args": [{"h": 1}], "ok": {"t": "inline me"}},
        )

    def test_cat_resolves_binary_blobs_as_base64(self):
        import hashlib

        payload = bytes([0xFF, 0xFE, 0x00])
        digest = hashlib.sha256(payload).hexdigest()
        blobs = self.dir / "blobs"
        blobs.mkdir()
        (blobs / digest).write_bytes(payload)
        tape = self.dir / "run.tape"
        write_full_tape(
            tape,
            ["app"],
            [entry(7, "read_file", [{"h": 1}], ok={"blob": digest})],
        )
        proc = self.run_os_tape("cat", str(tape), "--seq", "7")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(
            json.loads(proc.stdout)["ok"], {"b64": "//4A"}
        )

    def test_cat_missing_seq_fails(self):
        tape = self.dir / "run.tape"
        write_full_tape(tape, ["app"], [entry(1, "get_time", [], ok=1.0)])
        proc = self.run_os_tape("cat", str(tape), "--seq", "41")
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("no entry with seq 41", proc.stderr)

    def test_cat_needs_seq_option(self):
        tape = self.dir / "run.tape"
        write_full_tape(tape, ["app"], [entry(1, "get_time", [], ok=1.0)])
        proc = self.run_os_tape("cat", str(tape))
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("--seq", proc.stderr)


class KnownFnsTests(OsTapeTestCase):
    def test_cli_function_names_match_context(self):
        import re

        context = (REPO_ROOT / "Context" / "context.pen").read_text()
        pen_names = set(re.findall(r"_pen_os_([a-z_0-9]+)", context))
        cli_source = (REPO_ROOT / "bin" / "os-tape").read_text()
        match = re.search(r"KNOWN_FNS = frozenset\(\s*\{(.*?)\}", cli_source, re.S)
        self.assertIsNotNone(match, "KNOWN_FNS block not found in bin/os-tape")
        cli_names = set(re.findall(r'"([a-z_0-9]+)"', match.group(1)))
        self.assertEqual(cli_names, pen_names)
        self.assertEqual(len(cli_names), 32)


class StdlibTests(unittest.TestCase):
    """Portability: bin/os-tape runs on Python 3.9+ using stdlib only."""

    def cli_source(self):
        return (REPO_ROOT / "bin" / "os-tape").read_text()

    def test_stdlib_only_imports(self):
        import ast
        import sys

        tree = ast.parse(self.cli_source())
        imported = set()
        for node in ast.walk(tree):
            if isinstance(node, ast.Import):
                imported.update(alias.name.split(".")[0] for alias in node.names)
            elif isinstance(node, ast.ImportFrom) and node.module:
                imported.add(node.module.split(".")[0])
        self.assertTrue(imported, "import scan found nothing in bin/os-tape")
        stdlib = getattr(sys, "stdlib_module_names", None)
        if stdlib is None:  # Python 3.9 fallback: exact list unavailable.
            stdlib = frozenset(
                {
                    "argparse", "ast", "asyncio", "base64", "binascii", "collections",
                    "concurrent", "contextlib", "copy", "csv", "dataclasses", "datetime",
                    "difflib", "enum", "errno", "fnmatch", "functools", "getpass",
                    "glob", "hashlib", "hmac", "io", "itertools", "json", "logging",
                    "math", "mimetypes", "os", "pathlib", "platform", "random", "re",
                    "shlex", "shutil", "signal", "socket", "sqlite3", "ssl", "stat",
                    "string", "struct", "subprocess", "sys", "tempfile", "textwrap",
                    "threading", "time", "traceback", "typing", "unittest",
                    "urllib", "uuid", "warnings", "weakref", "zipfile", "zlib",
                }
                | set(sys.builtin_module_names)
            )
        third_party = imported - set(stdlib)
        self.assertEqual(third_party, set(), f"non-stdlib imports: {sorted(third_party)}")

    def test_stdlib_python39_grammar(self):
        import ast

        # Raises SyntaxError if the CLI uses grammar newer than Python 3.9
        # (match statements, X | Y annotations, ...).
        ast.parse(self.cli_source(), feature_version=(3, 9))


class EnvDocsTests(unittest.TestCase):
    def test_readme_env_vars_match_tape_rs(self):
        import re

        readme = (REPO_ROOT / "README.md").read_text()
        tape = (REPO_ROOT / "ffi" / "library" / "src" / "tape.rs").read_text()
        documented = set(re.findall(r"PEN_TAPE[A-Z_]*", readme))
        implemented = set(re.findall(r"PEN_TAPE[A-Z_]*", tape))
        self.assertTrue(documented, "no PEN_TAPE* names found in README")
        self.assertEqual(
            documented - implemented,
            set(),
            f"README documents unknown vars: {sorted(documented - implemented)}",
        )
        self.assertEqual(
            implemented - documented,
            set(),
            f"tape.rs vars missing from README: {sorted(implemented - documented)}",
        )


class CISetupTests(unittest.TestCase):
    """CI must pre-install Rust toolchains before any cargo runs.

    Regression test: parallel rustc shims under one cargo build raced
    rustup toolchain syncs on fresh runners (.partial rename failures),
    and clippy was missing from the minimal toolchain install.
    """

    def test_setup_pen_preinstalls_toolchains_first(self):
        action = (
            REPO_ROOT / ".github" / "actions" / "setup-pen" / "action.yaml"
        ).read_text()
        # `update`, not `install`: pre-existing stale toolchains must be
        # synced too, or parallel rustc shims race the sync later.
        self.assertIn("rustup update", action)
        self.assertIn("clippy", action)
        self.assertLess(
            action.index("rustup update"),
            action.index("cargo install"),
            "toolchain pre-sync must run before the first cargo invocation",
        )

    def test_workflow_disables_rustup_self_update(self):
        workflow = (REPO_ROOT / ".github" / "workflows" / "test.yaml").read_text()
        self.assertIn("RUSTUP_AUTO_SELF_UPDATE", workflow)

    def test_setup_pen_preinstalls_ffi_targets(self):
        action = (
            REPO_ROOT / ".github" / "actions" / "setup-pen" / "action.yaml"
        ).read_text()
        # Every pen package's ffi/ dir pins cross targets and pen build runs
        # those FFI builds concurrently; setup must pre-install the union
        # serially so no build-time sync can race.
        self.assertIn('"rustup", "target", "add"', action)

    def test_ffi_toolchains_pin_no_cross_targets(self):
        # os-tape's own FFI builds are native-only: demanding cross targets
        # would force slow downloads (and sync races) in every consumer's
        # build, as pen runs FFI builds concurrently.
        for name in ("ffi/library/rust-toolchain.toml", "ffi/application/rust-toolchain.toml"):
            text = (REPO_ROOT / name).read_text()
            self.assertNotIn("targets", text, f"{name} must not pin targets")


class DriftTests(unittest.TestCase):
    def upstream_files(self):
        script = (REPO_ROOT / "scripts" / "drift-check.sh").read_text()
        block = script.split("for f in")[1].split("; do")[0]
        return [token for token in block.replace("\\", " ").split() if token]

    def run_drift_check(self, upstream_base, cwd=None):
        if shutil.which("curl") is None:
            self.skipTest("curl not installed")
        env = dict(os.environ, UPSTREAM_BASE=upstream_base)
        return subprocess.run(
            [str(REPO_ROOT / "scripts" / "drift-check.sh")],
            env=env,
            cwd=cwd or REPO_ROOT,
            capture_output=True,
            text=True,
        )

    def stage_upstream(self, root: Path):
        for name in self.upstream_files():
            source = REPO_ROOT / name
            dest = root / name
            dest.parent.mkdir(parents=True, exist_ok=True)
            dest.write_bytes(source.read_bytes())

    def test_drift_check_passes_when_identical(self):
        with tempfile.TemporaryDirectory(prefix="drift-up-") as tmp:
            root = Path(tmp)
            self.stage_upstream(root)
            proc = self.run_drift_check(f"file://{root}")
            self.assertEqual(proc.returncode, 0, proc.stderr)
            self.assertIn("32/32", proc.stdout)

    def test_drift_check_fails_when_any_file_differs(self):
        with tempfile.TemporaryDirectory(prefix="drift-up-") as tmp:
            root = Path(tmp)
            self.stage_upstream(root)
            victim = root / "File.pen"
            victim.write_text(victim.read_text() + "\n# drifted\n")
            proc = self.run_drift_check(f"file://{root}")
            self.assertNotEqual(proc.returncode, 0)
            self.assertIn("File.pen", proc.stderr)
            self.assertIn("differs from upstream", proc.stderr)

    def test_drift_check_fails_when_upstream_unreachable(self):
        with tempfile.TemporaryDirectory(prefix="drift-up-") as tmp:
            missing = Path(tmp) / "does-not-exist"
            proc = self.run_drift_check(f"file://{missing}")
            self.assertNotEqual(proc.returncode, 0)
            self.assertIn("cannot fetch upstream", proc.stderr)


class RustTests(unittest.TestCase):
    @unittest.skipUnless(shutil.which("cargo"), "cargo not installed")
    def test_cargo_tests_pass_with_warnings_denied(self):
        env = dict(os.environ)
        env["RUSTFLAGS"] = "-D warnings"
        proc = subprocess.run(
            ["cargo", "test", "-p", "os-tape-ffi"],
            env=env,
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
        )
        self.assertEqual(proc.returncode, 0, proc.stderr[-2000:])


if __name__ == "__main__":
    unittest.main()
