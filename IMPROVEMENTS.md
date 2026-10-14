# os-tape improvements backlog (50 items)

Each item is one independently testable behavior. Categories vary per item.

1. [tape-format] `os-tape validate tape.tape` exits 0 on a well-formed tape and exits non-zero naming the first malformed line number on corrupt JSONL.
2. [tape-format] `os-tape validate` fails with the missing blob hash when a `{"blob": "<sha256>"}` reference has no matching `blobs/<sha256>` file.
3. [tape-format] `os-tape info tape.tape` prints call count, per-function counts, record duration, and total blob bytes, all matching a manual line count.
4. [tape-format] `os-tape cat tape.tape --seq 41` prints exactly the entry with `"seq": 41`, resolving blob references inline.
5. [tape-format] Recorded header line includes the os-tape version and replay refuses tapes whose major version differs, printing both versions.
6. [correctness] Replay of an empty-output program (`Exit` with no writes) exits with the recorded exit code and produces zero stdout bytes.
7. [correctness] A program that reads the same file twice replays both reads from one recording without requiring the file to exist.
8. [correctness] A program that opens two different files and reads them in reverse order across runs still replays (per-key FIFO, order-independent).
9. [correctness] Recording a program that exits non-zero stores the exit code, and strict replay exits with that same code.
10. [correctness] `GetTime` during replay returns the exact recorded timestamp string, never the wall clock.
11. [correctness] `Sleep` during replay returns in under 50 ms regardless of the recorded duration.
12. [correctness] A strict-replay call with no tape entry left exits non-zero and prints the function name, its arguments, and up to three closest tape entries.
13. [correctness] Stale-handle use (read after close) replays the recorded error text byte-for-byte instead of succeeding.
14. [correctness] A TCP client that connects, sends, and receives replays byte-identically with no listener present.
15. [correctness] A UDP client that sends one datagram and receives one reply replays byte-identically with no server present.
16. [diff-mode] `PEN_TAPE_OUTPUTS=diff` replay of unchanged code exits 0 and prints no diff output.
17. [diff-mode] `PEN_TAPE_OUTPUTS=diff` replay where one stdout line changed prints a unified diff containing exactly that line and exits non-zero.
18. [diff-mode] `PEN_TAPE_OUTPUTS=diff` replay ignores timing differences: a changed `Sleep` duration alone does not fail the diff.
19. [diff-mode] `PEN_TAPE_OUTPUTS=diff` compares file-write contents and reports the differing file path plus a unified diff.
20. [diff-mode] `PEN_TAPE_OUTPUTS=diff` treats an extra output call (not on tape) as a diff failure naming the unexpected call.
21. [robustness] Recording enforces the 512 MiB default size cap by aborting with a clear error naming the cap and `PEN_TAPE_MAX_BYTES` before exceeding it by more than one entry.
22. [robustness] `PEN_TAPE_MAX_BYTES=1M` parses the suffix and enforces a 1 MiB cap; an invalid value fails fast with a parse error.
23. [robustness] A malformed `PEN_TAPE` value (e.g. `frobnicate`) fails at startup with a usage message listing the valid forms.
24. [robustness] Recording to an unwritable tape path fails at startup with the OS error and path, before running the program.
25. [robustness] Replaying a tape whose file is missing fails with the tape path and a file-not-found message, not a panic backtrace.
26. [robustness] Concurrent recorders to the same tape path take an exclusive lock; the second fails with "tape locked" instead of interleaving lines.
27. [robustness] A recording interrupted by SIGINT flushes all complete entries so the partial tape still validates up to the last full line.
28. [CLI] `os-tape --help` exits 0 and documents all subcommands and flags shown by the README.
29. [CLI] `os-tape --version` prints the same version string stored in new tape headers.
30. [CLI] `os-tape record` without `--` separating app args exits 2 with a message showing the required `--` separator.
31. [CLI] `os-tape record --tape out.tape -- ./app` writes the tape to `out.tape` instead of the default location.
32. [CLI] `os-tape replay` with no tape argument exits 2 and prints usage to stderr.
33. [CLI] `os-tape check tapes/` prints one PASS/FAIL line per tape and exits non-zero if any tape fails.
34. [CLI] `os-tape check` on an empty directory exits 0 and prints "no tapes found" to stderr.
35. [CLI] `os-tape record` warns naming each uncovered dependency (`http`, `sql`, `random`) found in the app's `pen.json`.
36. [error-messages] Every record/replay failure message includes the `PEN_TAPE` mode and tape path involved.
37. [error-messages] Strict-replay mismatch output fits in 40 lines and always ends with a one-line suggestion (`re-record with os-tape record ...`).
38. [testing] `cargo test -p os-tape-ffi` passes with zero warnings under `RUSTFLAGS=-D warnings`.
39. [testing] A unit test asserts the wrapper table covers exactly the 32 foreign functions parsed from `Context/context.pen`.
40. [testing] A replay-concurrency test spawns 8 threads issuing interleaved reads and asserts replay succeeds in every order.
41. [testing] A blob-spill test records a 1 MiB read and asserts the tape line references `blobs/<sha256>` with matching file bytes.
42. [testing] An e2e test records `examples/cat`, deletes the input file, replays, and asserts byte-identical stdout.
43. [testing] A diff-mode e2e test changes one output line and asserts the diff output contains exactly that line.
44. [performance] Replaying the 10 s sleep fixture completes in under 1 s wall time.
45. [performance] Recording a program that writes 10,000 small lines completes in under 30 s and produces a tape under 5 MiB.
46. [portability] `os-tape` CLI runs on Python 3.9+ using stdlib only (CI job asserts no third-party imports).
47. [portability] Tape files use LF line endings and UTF-8 header text on all platforms (a test asserts no CR bytes in a fresh recording).
48. [security] Blob filenames are validated as 64 lowercase hex chars; a tape referencing `../evil` is rejected without writing outside `blobs/`.
49. [maintainability] `scripts/drift-check.sh` fails when any `.pen` file differs from upstream `packages/os@main` and passes when identical.
50. [docs] README documents every `PEN_TAPE*` env var, and a test asserts each documented var name appears in `tape.rs`.
