# os-tape: record a real run, replay it exactly

os-tape is a drop-in replacement for Pen's standard `os` system package. It
records every effect a program performs into a tape file, then replays that
tape without touching the disk, network, clock, or terminal. To use it,
change one line in `pen.json`:

```json
"Os": "git://github.com/<you>/os-tape"
```

The app source does not change: the compiler names the context field after
the `pen.json` key, so the code still says `ctx.Os`. A crash seen once in
production becomes a tape, and the tape becomes a regression test anyone can
run offline.

## Demos

Record once, replay exactly — even after the input file is deleted:

![record and replay demo](demos/demo-record-replay.gif)

A 10 s sleep replays instantly, and changed output shows up as a diff:

![instant replay and diff demo](demos/demo-diff.gif)

MP4 versions: [record/replay](demos/demo-record-replay.mp4),
[instant replay + diff](demos/demo-diff.mp4). Every frame shows real output:
regenerate them with `scripts/make-demos.sh` (needs the Pen toolchain,
`PEN_ROOT`, `ffmpeg`, and `script(1)`), which rebuilds the demo apps, records
the sessions with `script(1)`, renders 12 s clips, and verifies duration and
animation.

## Modes

Chosen at run time by `PEN_TAPE`; the same binary does all three:

| `PEN_TAPE`        | Behavior                                              |
| ----------------- | ----------------------------------------------------- |
| unset             | Behaves exactly like `os`                             |
| `record:<file>`   | Runs live and writes a tape                           |
| `replay:<file>`   | Serves every input from the tape                      |

Replay is strict by default: any call not on the tape stops the program with
a readable diff. With `PEN_TAPE_OUTPUTS=diff`, input calls are served from
the tape while output calls (stdout/stderr writes, file writes, sends,
creates, removes) are compared instead; os-tape prints a unified diff and
exits non-zero if they differ. Replay is fast: `Sleep` returns immediately
and `GetTime` returns the recorded value.

## CLI

```sh
os-tape record [--tape TAPE] -- ./app args...  # build and run in record mode
os-tape replay tape.tape             # replay one tape (app from the header)
os-tape replay --diff tape.tape      # replay, comparing outputs
os-tape check tapes/                 # replay every tape in diff mode (CI entry)
os-tape validate tape.tape           # check the tape is well-formed
os-tape info tape.tape               # calls, per-function counts, duration, blobs
os-tape cat tape.tape --seq 41       # print entry 41, blobs resolved inline
```

`record` warns when the app depends on system packages os-tape does not
cover (`http`, `sql`, `random`): those effects stay live during replay.
`record` writes to `tapes/<app>-<stamp>.tape` unless `--tape` (or `-o`)
names a path. `check` prints one `PASS`/`FAIL` line per tape and exits
non-zero when any tape differs; an empty directory prints "no tapes found"
and exits 0.

Prerequisites: the `pen` command, LLVM 16 (`llc`, `opt`, `llvm-ar`,
`llvm-config`), `turtle`, and `PEN_ROOT` pointing at a Pen checkout. CI
installs all of these (`.github/actions/setup-pen`).

## Tape format

JSONL. The first line is a header with the recorded argv, a snapshot of the
environment, the os-tape version, and the recording start time; each following
line is one call; the last line is a footer with the recording end time
(absent when the recording was interrupted). Replay refuses tapes whose
major os-tape version differs from the replayer's. Example call line:

```json
{"seq":41,"fn":"read_file","args":[{"h":3}],"ok":{"b64":"aGVsbG8K"}}
```

- Byte strings: valid UTF-8 as `{"t": "..."}`, anything else as
  `{"b64": "..."}`; values over 64 KB spill into `blobs/<sha256>` next to
  the tape (this applies to arguments as well as results).
- Handles (open files, TCP streams/listeners, UDP sockets) are logged as
  small integer ids (`{"h": 3}`); replay hands back stand-ins carrying only
  the id, never touching live I/O except re-emitting stdout/stderr so
  replayed terminal output is byte-identical.
- Errors are stored as `{"err": "<message>"}`, the same text the program saw.
- Replay matches calls by function name plus arguments, first-in-first-out
  per key, so concurrent tasks (`go`/`race`) replay regardless of thread
  order. `Exit` replays the recorded exit code.
- Recording has a size cap (default 512 MiB of tape + blobs,
  `PEN_TAPE_MAX_BYTES` overrides it, e.g. `PEN_TAPE_MAX_BYTES=1M`) that
  fails loudly instead of filling the disk. Two recorders racing for one
  tape path do not interleave: the second fails with "tape locked".
  Interrupted recordings flush every complete entry, so the partial tape
  still validates up to the last full line.

## Repository layout

This repo is a fork of upstream `packages/os`: the `.pen` modules and the
foreign names (`_pen_os_read_file` etc.) are byte-identical, and all behavior
lives in the Rust crate `ffi/library` (`src/tape.rs` holds the engine; each
of the 32 foreign functions gets a thin record/replay wrapper). Notable
additions over upstream:

- `ffi/library/src/tape.rs` — tape writer, per-key FIFO replay engine,
  output diff mode, handle id table
- `ffi/application/src/main.rs` — calls the tape finalizer after `main`
  returns so diff verdicts and flushes happen even when the app never calls
  `Exit`
- `bin/os-tape` — the `record`/`replay`/`check` CLI (Python 3, stdlib only)
- `tests/` — `test_os_tape.py` (CLI unit tests) and `e2e.py` (the PRD
  success criteria against the upstream examples)
- `scripts/drift-check.sh` — fails if any `.pen` file drifts from upstream
  or if the wrapper table drifts from `Context`

## Development

```sh
pen build                                            # typecheck the package
cargo clippy --all-targets --manifest-path ffi/library/Cargo.toml -- -D warnings
RUSTFLAGS="-D warnings" cargo test -p os-tape-ffi  # FFI unit tests, zero warnings
python3 -m unittest discover -s tests                # CLI unit tests
python3 tests/e2e.py                                 # success criteria (needs toolchain)
scripts/drift-check.sh                               # upstream drift + 32/32 wrappers
```

## Status and scope

Phase 1 covers the `os` package only (`http`, `sql`, `random` later). Out of
scope, as in the PRD: capturing child-process output of `RunCommand` (only
its result is recorded), reproducing exact `go`/`race` thread interleavings,
and tape editing tooling. See `prd.md` for the full plan. Open questions
carried from the PRD: strict-per-key vs global replay order under `os-sync`,
tapes beside `*.test.pen` with `pen test` integration, and a tape-aware
`random` package for v1.

## License

Dual-licensed under MIT ([LICENSE-MIT](LICENSE-MIT)) and Apache 2.0
([LICENSE-APACHE](LICENSE-APACHE)), matching the upstream
[pen-lang/pen](https://github.com/pen-lang/pen) `packages/os` this project
forks. The Rust crates carry `license = "MIT OR Apache-2.0"` in their
manifests.
