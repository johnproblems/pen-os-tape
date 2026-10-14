# os-tape: record a real run, replay it exactly

## Summary and novelty check

os-tape is a drop-in replacement for Pen's standard `os` system package. It records every effect a program performs into a tape file, then replays that tape without touching the disk, network, clock or terminal. To use it, change one line in `pen.json`:

```json
"Os": "git://github.com/<you>/os-tape"
```

The app source does not change, because the compiler names the context field after the `pen.json` key, so the code still says `ctx.Os` and `File'Write`. A crash seen once in production becomes a tape, and the tape becomes a regression test anyone can run offline.

**Why this only works cleanly in Pen.** In most languages, I/O can start from anywhere: a library, a global, a static initializer. In Pen, every effect goes through one record of 32 functions, `InnerContext` in `packages/os/Context/context.pen`, and each field is one foreign call into Rust. Wrap those 32 calls and you have caught everything the program can do to the outside world. Recording tools for other languages, like rr or HTTP "cassette" libraries, either work at the syscall level or cover only one protocol.

**Novelty check.** None of the following has a record/replay, fake-OS, or deterministic-testing tool or proposal:

- the 4 pen-lang repos
- the 38 open issues and the discussions
- GitHub search for Pen packages (none exist outside the main repo)

The closest item is issue #618, "Call trace for better debugging experience", which has only a title. Pen tests can't use the OS at all today: test functions take no `context`, and `Test'State` is the only help for faking effects.

## Goals, non-goals, success criteria

**Goals**

- Same Pen API as `os`, module for module, so swapping the URL is the only change.
- Three modes, chosen by the `PEN_TAPE` environment variable at run time:
  - unset: behaves exactly like `os`
  - `record:<file>`: writes a tape
  - `replay:<file>`: serves every input from the tape
- Replay is strict by default. Any call not on the tape stops the program with a readable diff.
- A diff replay mode (`PEN_TAPE_OUTPUTS=diff`) feeds the recorded inputs to changed code and reports how its outputs (stdout, file writes, sends) differ from the tape. That turns a tape into a behavior regression test.
- Replay is fast: `Sleep` returns immediately and `GetTime` returns the recorded value.

**Non-goals**

- Covering the `http`, `sql` and `random` system packages. They come later and use the same pattern.
- Capturing the output of child processes started by `RunCommand`. Only its result is recorded.
- Reproducing the exact thread interleaving of `go` and `race` (os-shuffle covers ordering).
- Editing tapes by hand. The tape is readable JSONL, but tooling for editing is out of scope.

**Success criteria**

- `examples/cat`, `examples/ls`, `examples/tcp-client` and `examples/udp-client` from the Pen repo record and replay with byte-identical stdout, with the network unplugged and the input files deleted.
- A program that sleeps 10 s replays in under 1 s.
- Changing one line of output in `examples/cat` makes `--outputs=diff` report exactly that line.
- Building the unchanged examples against os-tape needs no change except the `pen.json` URL.

## How it works

os-tape is a fork of `packages/os`. The Pen modules and the foreign names (`_pen_os_read_file` and the rest) stay identical. All the work happens in the Rust crate `ffi/library`, where each of the 32 `#[ffi::bindgen]` functions gets a thin wrapper.

**Recording.** Each wrapper calls the real function, then appends one JSON line to the tape:

```json
{"seq":41,"fn":"read_file","args":[{"h":3}],"ok":{"b64":"aGVsbG8K"}}
```

- Pen strings are byte strings. Valid UTF-8 is stored as text; anything else is stored as base64.
- Results over 64 KB go into a `blobs/` folder next to the tape, named by their SHA-256 hash.
- Errors are stored as `{"err": "<message>"}`, the same text the program would have seen.
- The tape's first line records the arguments, the environment variables the program read, and the os-tape version.

**Handles.** Open files, TCP streams and UDP sockets are opaque Rust objects passed back into Pen as `ffi::Any`. When recording, each new handle gets a small integer id, and later calls that use that handle log the id instead of the object. When replaying, os-tape returns a stand-in handle that carries only the id.

**Replaying.** Calls are matched by function name plus arguments, in first-in-first-out order per key, not by one global sequence. With the `os` runtime, `go` runs tasks on several threads, so two independent reads can happen in either order. Keying the queue keeps replay working anyway. Matching then works like this:

- A call with no tape entry left fails with the call, its arguments, and the three closest entries on the tape.
- `Exit` replays the recorded exit code.
- `GetTime` returns the recorded value, and `Sleep` returns at once.

**Output diff mode.** With `PEN_TAPE=replay:<file>` and `PEN_TAPE_OUTPUTS=diff`, input calls (reads, accepts, receives, time, arguments, environment) are served from the tape. Output calls (stdout and stderr writes, file writes, sends, creates, removes) are compared instead of being required to match. os-tape prints a unified diff of what the new code wrote against what the old code wrote, and exits non-zero if they differ.

**CLI.** A small `os-tape` script does three jobs:

- `os-tape record -- ./app args` builds and runs the app in record mode.
- `os-tape replay` replays one tape.
- `os-tape check tapes/` replays every tape in a folder in diff mode.

`check` is the CI entry point.

## Build plan, risks, open questions

**Build plan (about 20 hours)**

1. Fork `packages/os` into a new repo. Build `examples/cat` against it via a `git://` URL, unchanged. (1 h)
2. Write the tape writer, the JSONL schema, blob spill, and the header line. (3 h)
3. Add record wrappers for all 32 functions, generated by one Rust macro so the list cannot drift. (4 h)
4. Add the handle id table for files, TCP and UDP. (2 h)
5. Build the replay engine: per-key FIFO queues, stand-in handles, the mismatch report, fast `Sleep`, recorded `Exit`. (5 h)
6. Add output diff mode. (2 h)
7. Write the `os-tape record/replay/check` script and run the success-criteria examples in CI. (3 h)

**Risks**

| Risk | Mitigation |
| --- | --- |
| Drift from upstream `os` when it changes | `InnerContext` hasn't changed shape since 2023. A CI job diffs the fork's `.pen` files against upstream and fails on change. |
| Handles stored as `ffi::Any` may not be swappable for stand-ins without unsafe code | Prototype one handle type (`NormalFile`) in hour 1. The fallback is to make every handle a stand-in that wraps the real object when recording. |
| `RunCommand` children write straight to the terminal, outside the tape | Document it. A later version could capture child stdout through a pipe. |
| Large tapes from programs that read big files | Spilling blobs by hash keeps the JSONL small. A size cap with a clear error stops runaway tapes. |
| An app that also lists `http` or `sql` still performs those effects live during replay | Print a warning naming every system package in the app that os-tape does not cover. |

**Open questions**

- Should replay order be strict per key (the current plan) or fully global when the app uses `os-sync`, which is deterministic?
- Should tapes be committed next to `*.test.pen` files, with `pen test` taught to run `os-tape check`? That needs an upstream change to the test runner.
- Is a tape-aware `random` package worth adding in v1? It is tiny, and without it, apps that use random numbers won't replay the same way.

## Sources

- [`packages/os/Context/context.pen`](https://github.com/pen-lang/pen/blob/main/packages/os/Context/context.pen): the 32-field `InnerContext`
- [`lib/hir-mir/src/main_function.rs`](https://github.com/pen-lang/pen/blob/main/lib/hir-mir/src/main_function.rs): how `context` is built from `pen.json` keys
- [Writing system packages](https://pen-lang.org/advanced-features/writing-system-packages/)
- [Testing guide](https://pen-lang.org/guides/testing/)
- [Issue #618: Call trace for better debugging experience](https://github.com/pen-lang/pen/issues/618)
