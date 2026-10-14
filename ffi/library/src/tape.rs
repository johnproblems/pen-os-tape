//! Record/replay tape engine for os-tape.
//!
//! The engine interposes on the 32 foreign functions of the `os` system
//! package. Its behavior is selected at run time by `PEN_TAPE`:
//!
//! - unset: pass every call through to the real OS (behaves exactly like `os`).
//! - `record:<file>`: run the real OS and append one JSON line per call.
//! - `replay:<file>`: serve every call from the tape, touching nothing.
//!
//! With `replay:<file>` plus `PEN_TAPE_OUTPUTS=diff`, input calls are served
//! from the tape while output calls are compared against it; differences are
//! reported as a unified diff and the process exits non-zero.

use fs2::FileExt;
use serde_json::{Value, json};
use sha2::Digest;
use std::{
    collections::{HashMap, VecDeque},
    fs::File,
    io::{BufRead, BufReader, BufWriter, Write},
    path::PathBuf,
    process::exit,
    sync::{LazyLock, Mutex, MutexGuard},
};

/// Tape format version written into the header line.
const TAPE_FORMAT_VERSION: u64 = 1;
/// Byte strings larger than this spill into `blobs/<sha256>` next to the tape.
const BLOB_THRESHOLD: usize = 64 * 1024;
/// Default cap on tape + blob bytes during recording.
const DEFAULT_MAX_BYTES: u64 = 512 * 1024 * 1024;
/// Environment variable selecting the engine mode.
const TAPE_ENV_VAR: &str = "PEN_TAPE";
/// Environment variable selecting diff mode for replay.
const OUTPUTS_ENV_VAR: &str = "PEN_TAPE_OUTPUTS";
/// Environment variable overriding the recording size cap.
const MAX_BYTES_ENV_VAR: &str = "PEN_TAPE_MAX_BYTES";

// The single source of truth for the wrapped foreign functions (without the
// `_pen_os_` prefix) and their input/output classification. The wrappers,
// replay matching, and the drift check script all derive from this table so
// the list of 32 cannot drift.
macro_rules! tape_fns {
    ($( $name:ident : $kind:ident ),* $(,)?) => {
        /// Names of all wrapped foreign functions.
        pub const FN_NAMES: &[&str] = &[ $( stringify!($name) ),* ];

        /// Whether `name` is an output call (compared, not matched, in diff mode).
        pub fn is_output_fn(name: &str) -> bool {
            $( tape_fns!(@is_output name, $name, $kind) )||*
        }

        /// Whether `name` is a known wrapped function.
        pub fn is_known_fn(name: &str) -> bool {
            FN_NAMES.contains(&name)
        }
    };
    (@is_output $param:ident, $name:ident, output) => {
        $param == stringify!($name)
    };
    (@is_output $param:ident, $name:ident, input) => {
        false
    };
}

tape_fns! {
    read_stdin: input,
    read_limit_stdin: input,
    write_stdout: output,
    write_stderr: output,
    open_file: input,
    read_file: input,
    read_limit_file: input,
    write_file: output,
    copy_file: output,
    move_file: output,
    remove_file: output,
    read_directory: input,
    create_directory: output,
    remove_directory: output,
    get_arguments: input,
    get_environment_variable: input,
    read_metadata: input,
    tcp_bind: input,
    tcp_connect: input,
    tcp_accept: input,
    tcp_receive: input,
    tcp_send: output,
    udp_bind: input,
    udp_connect: input,
    udp_receive: input,
    udp_receive_from: input,
    udp_send: output,
    udp_send_to: output,
    get_time: input,
    sleep: input,
    exit: input,
    run_command: input,
}

/// What the engine tells a wrapper to do with the current call.
pub enum ReplayOutcome {
    /// No replay entry applies: run the real function (pass-through and
    /// record modes, where the wrapper records afterwards).
    Passthrough,
    /// Use this recorded result instead of running the real function.
    Return(Result<Value, String>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Passthrough,
    Record,
    Replay,
}

struct RecordState {
    writer: BufWriter<File>,
    path: PathBuf,
    blobs_dir: PathBuf,
    next_seq: u64,
    next_handle: u32,
    bytes_written: u64,
    max_bytes: u64,
}

struct Entry {
    seq: u64,
    func: String,
    args: Value,
    result: Result<Value, String>,
    consumed: bool,
}

struct ReplayState {
    path: PathBuf,
    blobs_dir: PathBuf,
    entries: Vec<Entry>,
    /// Per-key FIFO queues of entry indexes; the key is function name plus
    /// canonical argument JSON so independent concurrent calls match in any
    /// order.
    queues: HashMap<String, VecDeque<usize>>,
    /// Handle id to file path, learned from consumed `open_file` entries so
    /// diff mode can name the file a `write_file` call wrote to.
    handle_paths: HashMap<u32, String>,
    diff: bool,
    diffs: Vec<String>,
    diff_failed: bool,
}

struct Engine {
    mode: Mode,
    record: Option<RecordState>,
    replay: Option<ReplayState>,
}

static ENGINE: LazyLock<Mutex<Engine>> = LazyLock::new(|| Mutex::new(Engine::init()));

fn lock_engine() -> MutexGuard<'static, Engine> {
    ENGINE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Queue key for a call: function name plus canonical argument JSON.
fn key_for(func: &str, args: &Value) -> String {
    let mut key = String::with_capacity(func.len() + args.to_string().len() + 1);
    key.push_str(func);
    key.push('\0');
    key.push_str(&args.to_string());
    key
}

/// Print a message to stderr and terminate with a failure status.
///
/// Every record/replay failure names the `PEN_TAPE` mode and tape path that
/// were in force (the spec itself is `record:<file>` or `replay:<file>`, so it
/// carries both). The context is a prefix so multi-line reports (notably the
/// strict-replay mismatch report) still end with their suggestion line.
pub fn fatal(message: &str) -> ! {
    match std::env::var(TAPE_ENV_VAR).ok() {
        Some(spec) if !spec.is_empty() => {
            eprintln!("os-tape: [{TAPE_ENV_VAR}={spec}] {message}");
        }
        _ => {
            eprintln!("os-tape: {message}");
        }
    }
    exit(1);
}

/// Current wall-clock time as milliseconds since the Unix epoch.
fn unix_millis() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as f64)
        .unwrap_or(0.0)
}

/// Major component of a `major.minor.patch` version string.
fn major_of(version: &str) -> Option<u64> {
    version.split('.').next().and_then(|s| s.parse().ok())
}

#[derive(PartialEq, Eq, Debug)]
enum VersionCheck {
    Compatible,
    Incompatible { recorded: String, current: String },
    Unknown,
}

/// Whether a tape recorded by `recorded` may replay under `current`: the
/// major versions must match. A missing or unparseable recorded version is
/// `Unknown` (warn and try) so pre-version tapes still load.
fn check_version(recorded: Option<&str>, current: &str) -> VersionCheck {
    match recorded.and_then(|version| major_of(version).map(|major| (version, major))) {
        Some((version, recorded_major)) => match major_of(current) {
            Some(current_major) if recorded_major == current_major => VersionCheck::Compatible,
            _ => VersionCheck::Incompatible {
                recorded: version.to_owned(),
                current: current.to_owned(),
            },
        },
        None => VersionCheck::Unknown,
    }
}

/// Parse a `PEN_TAPE_MAX_BYTES` value: a plain byte count or a count with a
/// `K`/`M`/`G`/`T` suffix (an optional trailing `B` and any letter case are
/// allowed), so `1M` means one mebibyte.
fn parse_max_bytes(raw: &str) -> Result<u64, String> {
    let invalid = || {
        format!(
            "invalid {MAX_BYTES_ENV_VAR} value {raw:?}: \
             want a byte count like 1048576, 512K, 1M, or 2G"
        )
    };
    let text = raw.trim();
    let digits_len = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (digits, suffix) = text.split_at(digits_len);
    if digits.is_empty() {
        return Err(invalid());
    }
    let base: u64 = digits.parse().map_err(|_| invalid())?;
    let multiplier: u64 = match suffix.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "K" | "KB" => 1024,
        "M" | "MB" => 1024 * 1024,
        "G" | "GB" => 1024 * 1024 * 1024,
        "T" | "TB" => 1024 * 1024 * 1024 * 1024,
        _ => return Err(invalid()),
    };
    base.checked_mul(multiplier).ok_or_else(invalid)
}

/// A byte count in the largest binary unit that divides it evenly.
fn human_bytes(count: u64) -> String {
    for (unit, name) in [
        (1024 * 1024 * 1024 * 1024, "TiB"),
        (1024 * 1024 * 1024, "GiB"),
        (1024 * 1024, "MiB"),
        (1024, "KiB"),
    ] {
        if count >= unit && count.is_multiple_of(unit) {
            return format!("{} {name}", count / unit);
        }
    }
    format!("{count} bytes")
}

fn size_cap_message(path: &std::path::Path, max_bytes: u64) -> String {
    format!(
        "tape {} exceeded size cap of {} bytes ({}); \
         set {MAX_BYTES_ENV_VAR} to raise it",
        path.display(),
        max_bytes,
        human_bytes(max_bytes),
    )
}

impl Engine {
    fn init() -> Self {
        match std::env::var(TAPE_ENV_VAR).ok() {
            None => Self {
                mode: Mode::Passthrough,
                record: None,
                replay: None,
            },
            Some(spec) => {
                if let Some(path) = spec.strip_prefix("record:") {
                    let path = PathBuf::from(path);
                    Self::open_record(path)
                } else if let Some(path) = spec.strip_prefix("replay:") {
                    let path = PathBuf::from(path);
                    let diff = std::env::var(OUTPUTS_ENV_VAR).as_deref() == Ok("diff");
                    Self::open_replay(path, diff)
                } else {
                    fatal(&format!(
                        "invalid {TAPE_ENV_VAR} value {spec:?}: want \"record:<file>\" \
                         or \"replay:<file>\""
                    ));
                }
            }
        }
    }

    fn blobs_dir_for(tape_path: &std::path::Path) -> PathBuf {
        tape_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(|parent| parent.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
            .join("blobs")
    }

    fn max_bytes() -> u64 {
        match std::env::var(MAX_BYTES_ENV_VAR) {
            Ok(raw) => match parse_max_bytes(&raw) {
                Ok(limit) => limit,
                Err(message) => fatal(&message),
            },
            Err(_) => DEFAULT_MAX_BYTES,
        }
    }

    fn open_record(path: PathBuf) -> Self {
        let blobs_dir = Self::blobs_dir_for(&path);
        if let Err(error) = std::fs::create_dir_all(&blobs_dir) {
            fatal(&format!(
                "cannot create blobs directory {}: {error}",
                blobs_dir.display()
            ));
        }
        // Open without truncating first: the exclusive lock below decides who
        // owns the tape, and only the winner truncates it. Truncating before
        // locking would let a losing recorder wipe the winner's header.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path);
        let file = match file {
            Ok(file) => file,
            Err(error) => fatal(&format!("cannot create tape {}: {error}", path.display())),
        };
        if let Err(error) = file.try_lock_exclusive() {
            fatal(&format!(
                "tape locked: {} is already being recorded by another process ({error}); \
                 use a different tape path",
                path.display()
            ));
        }
        let mut file = file;
        if let Err(error) = file.set_len(0) {
            fatal(&format!("cannot truncate tape {}: {error}", path.display()));
        }
        let header = json!({
            "header": true,
            "v": TAPE_FORMAT_VERSION,
            "os_tape": env!("CARGO_PKG_VERSION"),
            "start_time": unix_millis(),
            "argv": std::env::args().collect::<Vec<_>>(),
            "env": std::env::vars().collect::<HashMap<String, String>>(),
        });
        let mut line = header.to_string();
        line.push('\n');
        if let Err(error) = file.write_all(line.as_bytes()) {
            fatal(&format!("cannot write tape {}: {error}", path.display()));
        }
        Self {
            mode: Mode::Record,
            record: Some(RecordState {
                writer: BufWriter::new(file),
                path,
                blobs_dir,
                next_seq: 1,
                next_handle: 1,
                bytes_written: line.len() as u64,
                max_bytes: Self::max_bytes(),
            }),
            replay: None,
        }
    }

    fn open_replay(path: PathBuf, diff: bool) -> Self {
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) => fatal(&format!("cannot open tape {}: {error}", path.display())),
        };
        let blobs_dir = Self::blobs_dir_for(&path);
        let mut entries = Vec::new();
        let mut queues: HashMap<String, VecDeque<usize>> = HashMap::new();
        let mut lines = BufReader::new(file).lines();
        let header = lines.next();
        match header {
            Some(Ok(line)) => {
                let value: Value = match serde_json::from_str(&line) {
                    Ok(value) => value,
                    Err(error) => fatal(&format!(
                        "cannot parse tape header in {}: {error}",
                        path.display()
                    )),
                };
                if value.get("header") != Some(&Value::Bool(true)) {
                    fatal(&format!(
                        "bad tape header in {}: first line is not a header",
                        path.display()
                    ));
                }
                match check_version(
                    value.get("os_tape").and_then(Value::as_str),
                    env!("CARGO_PKG_VERSION"),
                ) {
                    VersionCheck::Compatible => {}
                    VersionCheck::Incompatible { recorded, current } => fatal(&format!(
                        "cannot replay tape {}: recorded with os-tape {recorded}, \
                         replaying with os-tape {current} (major version differs; \
                         re-record the tape)",
                        path.display()
                    )),
                    VersionCheck::Unknown => eprintln!(
                        "os-tape: warning: tape {} has no readable os-tape version; trying anyway",
                        path.display()
                    ),
                }
            }
            Some(Err(error)) => fatal(&format!("cannot read tape {}: {error}", path.display())),
            None => fatal(&format!("empty tape {}", path.display())),
        }
        for line in lines {
            let line = match line {
                Ok(line) => line,
                Err(error) => fatal(&format!("cannot read tape {}: {error}", path.display())),
            };
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(&line) {
                Ok(value) => value,
                Err(error) => fatal(&format!(
                    "cannot parse tape line in {}: {error}",
                    path.display()
                )),
            };
            if value.get("footer") == Some(&Value::Bool(true)) {
                continue;
            }
            let Some(seq) = value.get("seq").and_then(Value::as_u64) else {
                fatal(&format!(
                    "bad tape entry in {}: missing seq",
                    path.display()
                ));
            };
            let Some(func) = value.get("fn").and_then(Value::as_str) else {
                fatal(&format!("bad tape entry in {}: missing fn", path.display()));
            };
            if !is_known_fn(func) {
                fatal(&format!(
                    "bad tape entry in {}: unknown function {func:?}",
                    path.display()
                ));
            }
            let args = value.get("args").cloned().unwrap_or(Value::Null);
            let result = if let Some(ok) = value.get("ok") {
                Ok(ok.clone())
            } else if let Some(message) = value.get("err").and_then(Value::as_str) {
                Err(message.to_owned())
            } else {
                fatal(&format!(
                    "bad tape entry in {}: missing ok/err",
                    path.display()
                ));
            };
            let index = entries.len();
            queues
                .entry(key_for(func, &args))
                .or_default()
                .push_back(index);
            entries.push(Entry {
                seq,
                func: func.to_owned(),
                args,
                result,
                consumed: false,
            });
        }
        Self {
            mode: Mode::Replay,
            record: None,
            replay: Some(ReplayState {
                path,
                blobs_dir,
                entries,
                queues,
                handle_paths: HashMap::new(),
                diff,
                diffs: Vec::new(),
                diff_failed: false,
            }),
        }
    }
}

/// SHA-256 digest of a byte string, as lowercase hex: the blob filename.
fn blob_digest(bytes: &[u8]) -> String {
    format!("{:x}", sha2::Sha256::digest(bytes))
}

/// Encode a Pen byte string: valid UTF-8 as text, anything else as base64,
/// and anything over 64 KB as a blob reference.
///
/// The spill decision is identical in record and replay modes (only the file
/// write is record-only): replay matches calls by their encoded arguments, so
/// a large argument recorded as `{"blob": ...}` must encode the same way when
/// replaying or strict matching can never hit. In pass-through mode the value
/// is never matched or stored, so it stays inline.
pub fn enc_bytes(bytes: &[u8]) -> Value {
    if bytes.len() > BLOB_THRESHOLD {
        let digest = blob_digest(bytes);
        let mut engine = lock_engine();
        if engine.mode == Mode::Record {
            let record = engine.record.as_mut().expect("record state");
            let path = record.blobs_dir.join(&digest);
            if !path.exists() {
                match std::fs::write(&path, bytes) {
                    Ok(()) => {
                        record.bytes_written += bytes.len() as u64;
                        check_size_cap(record);
                    }
                    Err(error) => {
                        if error.kind() != std::io::ErrorKind::AlreadyExists {
                            fatal(&format!("cannot write blob {}: {error}", path.display()));
                        }
                    }
                }
            }
        }
        if engine.mode != Mode::Passthrough {
            return json!({ "blob": digest });
        }
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => json!({ "t": text }),
        Err(_) => {
            json!({ "b64": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes) })
        }
    }
}

/// Whether `digest` is a safe blob filename: exactly 64 lowercase hex chars
/// (a SHA-256 digest). Anything else — notably `..` segments like `../evil`
/// — is rejected so a hostile tape can neither read nor write outside
/// `blobs/`. (Recording only ever writes computed digests, which always
/// qualify; this gate protects replay.)
pub fn is_valid_blob_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Decode a value produced by [`enc_bytes`], loading blobs in replay mode.
pub fn dec_bytes(value: &Value) -> Result<Vec<u8>, String> {
    if let Some(text) = value.get("t").and_then(Value::as_str) {
        return Ok(text.as_bytes().to_vec());
    }
    if let Some(encoded) = value.get("b64").and_then(Value::as_str) {
        return base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded)
            .map_err(|error| format!("bad base64 in tape: {error}"));
    }
    if let Some(digest) = value.get("blob").and_then(Value::as_str) {
        if !is_valid_blob_digest(digest) {
            return Err(format!(
                "bad blob digest {digest:?}: want 64 lowercase hex chars"
            ));
        }
        let engine = lock_engine();
        let replay = engine.replay.as_ref().ok_or_else(|| {
            "blob reference outside replay mode (encode/decode mismatch)".to_owned()
        })?;
        let path = replay.blobs_dir.join(digest);
        return std::fs::read(&path)
            .map_err(|error| format!("cannot read blob {}: {error}", path.display()));
    }
    Err(format!("bad byte-string value in tape: {value}"))
}

/// Encode a list of byte strings as a JSON array.
pub fn enc_str_list(items: &[Vec<u8>]) -> Value {
    Value::Array(items.iter().map(|item| enc_bytes(item)).collect())
}

/// Decode a JSON array of byte strings.
pub fn dec_str_list(value: &Value) -> Result<Vec<Vec<u8>>, String> {
    let Value::Array(items) = value else {
        return Err(format!("bad string-list value in tape: {value}"));
    };
    items.iter().map(dec_bytes).collect()
}

/// Encode a handle reference.
pub fn enc_handle(id: u32) -> Value {
    json!({ "h": id })
}

/// Decode a handle reference.
pub fn dec_handle(value: &Value) -> Result<u32, String> {
    value
        .get("h")
        .and_then(Value::as_u64)
        .and_then(|id| u32::try_from(id).ok())
        .ok_or_else(|| format!("bad handle value in tape: {value}"))
}

/// Decode a JSON number as f64.
pub fn dec_number(value: &Value) -> Result<f64, String> {
    value
        .as_f64()
        .ok_or_else(|| format!("bad number value in tape: {value}"))
}

fn check_size_cap(record: &RecordState) {
    if record.bytes_written > record.max_bytes {
        fatal(&size_cap_message(&record.path, record.max_bytes));
    }
}

/// Append the closing footer line (recording end time) and flush the tape.
/// Replay skips footer lines; `os-tape info` reads the record duration from
/// the header `start_time` and this `end_time`.
fn write_footer(record: &mut RecordState) {
    let footer = json!({
        "footer": true,
        "end_time": unix_millis(),
        "calls": record.next_seq - 1,
    });
    let mut text = footer.to_string();
    text.push('\n');
    if let Err(error) = record.writer.write_all(text.as_bytes()) {
        fatal(&format!(
            "cannot write tape {}: {error}",
            record.path.display()
        ));
    }
    record.bytes_written += text.len() as u64;
    if let Err(error) = record.writer.flush() {
        fatal(&format!(
            "cannot flush tape {}: {error}",
            record.path.display()
        ));
    }
}

/// Append one entry to the tape. No-op unless recording.
pub fn record(func: &str, args: Value, result: Result<Value, String>) {
    let mut engine = lock_engine();
    if engine.mode != Mode::Record {
        return;
    }
    let record = engine.record.as_mut().expect("record state");
    let seq = record.next_seq;
    record.next_seq += 1;
    let mut line = json!({
        "seq": seq,
        "fn": func,
        "args": args,
    });
    match &result {
        Ok(ok) => line["ok"] = ok.clone(),
        Err(message) => line["err"] = Value::String(message.clone()),
    }
    let mut text = line.to_string();
    text.push('\n');
    if let Err(error) = record.writer.write_all(text.as_bytes()) {
        fatal(&format!(
            "cannot write tape {}: {error}",
            record.path.display()
        ));
    }
    // Flush every entry so a recording interrupted by SIGINT still validates
    // up to the last full line: only complete entries ever reach the tape.
    if let Err(error) = record.writer.flush() {
        fatal(&format!(
            "cannot flush tape {}: {error}",
            record.path.display()
        ));
    }
    record.bytes_written += text.len() as u64;
    check_size_cap(record);
}

/// Allocate a fresh handle id for a newly created live handle. In
/// pass-through mode handles carry the dummy id 0; replay uses recorded ids.
pub fn alloc_handle() -> u32 {
    let mut engine = lock_engine();
    if engine.mode != Mode::Record {
        return 0;
    }
    let record = engine.record.as_mut().expect("record state");
    let id = record.next_handle;
    record.next_handle += 1;
    id
}

impl ReplayState {
    /// Pop the recorded result for one call. `Ok` carries the recorded
    /// outcome; `Err` carries the mismatch report instead of exiting, so
    /// tests can drive matching (including concurrently) without dying.
    fn take(&mut self, func: &str, args: &Value) -> Result<Result<Value, String>, String> {
        if self.diff && is_output_fn(func) {
            return Ok(diff_output_take(self, func, args));
        }
        if self.diff && func == "sleep" {
            // Timing is not compared in diff mode: a changed Sleep duration
            // alone never fails the diff.
            return Ok(diff_sleep_take(self, args));
        }
        let key = key_for(func, args);
        match self.queues.get_mut(&key).and_then(VecDeque::pop_front) {
            Some(index) => {
                let (result, entry_args) = {
                    let entry = &mut self.entries[index];
                    entry.consumed = true;
                    (entry.result.clone(), entry.args.clone())
                };
                if func == "open_file" {
                    // Remember which path each handle id refers to for diff mode.
                    let handle = result
                        .as_ref()
                        .ok()
                        .and_then(|value| dec_handle(value).ok());
                    if let (Some(handle), Some(path)) =
                        (handle, entry_args.get(0).and_then(bytes_to_text))
                    {
                        self.handle_paths.insert(handle, path);
                    }
                }
                Ok(result)
            }
            None => Err(mismatch_report(self, func, args)),
        }
    }
}

/// Decide what a wrapper should do for the current call. In replay modes this
/// pops the matching entry (or reports a mismatch); otherwise it returns
/// [`ReplayOutcome::Passthrough`] so the wrapper runs the real function.
pub fn replay_take(func: &str, args: &Value) -> ReplayOutcome {
    let mut engine = lock_engine();
    if engine.mode != Mode::Replay {
        return ReplayOutcome::Passthrough;
    }
    let replay = engine.replay.as_mut().expect("replay state");
    match replay.take(func, args) {
        Ok(result) => ReplayOutcome::Return(result),
        Err(report) => fatal(&report),
    }
}

/// Serve a `sleep` call in diff mode: consume the exact entry when one
/// matches, else any remaining `sleep` entry regardless of its recorded
/// duration. A `sleep` with no entry left is timing only, so it passes.
fn diff_sleep_take(replay: &mut ReplayState, args: &Value) -> Result<Value, String> {
    let key = key_for("sleep", args);
    if let Some(index) = replay.queues.get_mut(&key).and_then(VecDeque::pop_front) {
        let entry = &mut replay.entries[index];
        entry.consumed = true;
        return entry.result.clone();
    }
    let sibling = replay
        .entries
        .iter()
        .enumerate()
        .find(|(_, entry)| !entry.consumed && entry.func == "sleep")
        .map(|(index, entry)| (index, entry.args.clone(), entry.result.clone()));
    match sibling {
        Some((index, sibling_args, sibling_result)) => {
            let sibling_key = key_for("sleep", &sibling_args);
            if let Some(queue) = replay.queues.get_mut(&sibling_key) {
                queue.retain(|queued| *queued != index);
            }
            replay.entries[index].consumed = true;
            sibling_result
        }
        None => Ok(Value::Null),
    }
}

/// Decode an [`enc_bytes`] value to text without touching the engine lock, so
/// replay matching can read paths out of `open_file` arguments. Blob
/// references (only huge values) resolve to `None`.
fn bytes_to_text(value: &Value) -> Option<String> {
    if let Some(text) = value.get("t").and_then(Value::as_str) {
        return Some(text.to_owned());
    }
    value
        .get("b64")
        .and_then(Value::as_str)
        .and_then(|encoded| {
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded).ok()
        })
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// Render a JSON value for the mismatch report on one truncated line: huge
/// arguments (megabyte blobs render inline) must not blow the report past its
/// 40-line budget.
fn report_arg(args: &Value) -> String {
    const LIMIT: usize = 2000;
    let text = args.to_string().replace('\n', "\\n");
    if text.len() > LIMIT {
        let mut end = LIMIT;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…[truncated {} chars]", &text[..end], text.len() - end)
    } else {
        text
    }
}

fn mismatch_report(replay: &ReplayState, func: &str, args: &Value) -> String {
    let mut report = format!(
        "replay mismatch on tape {} (mode=replay): no entry left for call `{func}` with args {}\n",
        replay.path.display(),
        report_arg(args),
    );
    let remaining: Vec<&Entry> = replay
        .entries
        .iter()
        .filter(|entry| !entry.consumed)
        .collect();
    report.push_str(&format!(
        "{} of {} entries consumed; {} remaining\n",
        replay.entries.len() - remaining.len(),
        replay.entries.len(),
        remaining.len(),
    ));
    let same_fn: Vec<&&Entry> = remaining
        .iter()
        .filter(|entry| entry.func == func)
        .take(3)
        .collect();
    if same_fn.is_empty() {
        report.push_str(&format!(
            "no remaining entries for `{func}`; nearest entries:\n"
        ));
        for entry in remaining.iter().take(3) {
            report.push_str(&format!(
                "  seq={} fn={} args={}\n",
                entry.seq,
                entry.func,
                report_arg(&entry.args)
            ));
        }
    } else {
        report.push_str("closest remaining entries:\n");
        for entry in same_fn {
            report.push_str(&format!(
                "  seq={} args={}\n",
                entry.seq,
                report_arg(&entry.args)
            ));
        }
    }
    report.push_str(&format!(
        "suggestion: re-record with os-tape record --tape {} -- ./app [args...]",
        replay.path.display()
    ));
    debug_assert!(
        report.lines().count() <= 40,
        "mismatch report exceeded 40 lines"
    );
    report
}

/// Pop the expected entry for an output call in diff mode, compare it with
/// the actual call, and stash a unified diff on mismatch. Always returns a
/// result so the program runs to completion and every difference is reported.
fn diff_output_take(replay: &mut ReplayState, func: &str, args: &Value) -> Result<Value, String> {
    let key = key_for(func, args);
    let index = replay.queues.get_mut(&key).and_then(VecDeque::pop_front);
    match index {
        Some(index) => {
            let entry = &mut replay.entries[index];
            entry.consumed = true;
            entry.result.clone()
        }
        None => {
            // Look for an expected entry for the same function with different
            // arguments to diff against; otherwise this is an extra output.
            let sibling = replay
                .entries
                .iter()
                .enumerate()
                .find(|(_, entry)| !entry.consumed && entry.func == func)
                .map(|(index, entry)| {
                    (
                        index,
                        entry.func.clone(),
                        entry.args.clone(),
                        entry.seq,
                        entry.result.clone(),
                    )
                });
            match sibling {
                Some((index, sibling_func, sibling_args, sibling_seq, sibling_result)) => {
                    // Remove it from its own queue so it is not reused.
                    let sibling_key = key_for(&sibling_func, &sibling_args);
                    if let Some(queue) = replay.queues.get_mut(&sibling_key) {
                        queue.retain(|queued| *queued != index);
                    }
                    replay.entries[index].consumed = true;
                    // File writes compare the written contents and name the
                    // differing file; other outputs diff the whole arguments.
                    let (label, expected, actual) = if func == "write_file" {
                        let target = sibling_args
                            .get(0)
                            .and_then(|value| dec_handle(value).ok())
                            .and_then(|id| replay.handle_paths.get(&id).cloned())
                            .map(|path| format!("file {path}"))
                            .unwrap_or_else(|| {
                                format!("handle {}", sibling_args.get(0).unwrap_or(&Value::Null))
                            });
                        (
                            format!("`{func}` {target}"),
                            render_value(sibling_args.get(1).unwrap_or(&Value::Null)),
                            render_value(args.get(1).unwrap_or(&Value::Null)),
                        )
                    } else {
                        (
                            format!("`{func}`"),
                            render_value(&sibling_args),
                            render_value(args),
                        )
                    };
                    let diff = unified_diff(
                        &format!("tape {} seq={sibling_seq} fn={func}", replay.path.display()),
                        "actual",
                        &expected,
                        &actual,
                    );
                    replay.diffs.push(format!(
                        "--- output differs: {label} (tape seq={sibling_seq})\n{diff}"
                    ));
                    replay.diff_failed = true;
                    sibling_result
                }
                None => {
                    replay.diffs.push(format!(
                        "--- extra output: `{func}` with args {} has no tape entry\n+++ actual\n{}",
                        args,
                        render_value(args)
                            .lines()
                            .map(|line| format!("+{line}"))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ));
                    replay.diff_failed = true;
                    Ok(hypothetical_ok(func, args))
                }
            }
        }
    }
}

/// A plausible result for an output call with no tape entry, so the program
/// can continue in diff mode. Write/send calls report the payload length.
fn hypothetical_ok(func: &str, args: &Value) -> Value {
    // Payload positions: write_stdout/write_stderr take only bytes (args[0]);
    // the handle-based writes/sends take bytes second (args[1]).
    let payload_index = match func {
        "write_stdout" | "write_stderr" => Some(0),
        "write_file" | "tcp_send" | "udp_send" | "udp_send_to" => Some(1),
        _ => None,
    };
    match payload_index {
        Some(index) => {
            let payload = args.get(index);
            let len = payload
                .and_then(|value| {
                    value
                        .get("t")
                        .and_then(Value::as_str)
                        .map(|text| text.len())
                        .or_else(|| {
                            value
                                .get("b64")
                                .and_then(Value::as_str)
                                .and_then(|encoded| {
                                    base64::Engine::decode(
                                        &base64::engine::general_purpose::STANDARD,
                                        encoded,
                                    )
                                    .ok()
                                    .map(|bytes| bytes.len())
                                })
                        })
                        .or_else(|| value.get("blob").and_then(Value::as_str).map(|_| 0))
                })
                .unwrap_or(0);
            json!(len as f64)
        }
        None => Value::Null,
    }
}

/// Render a recorded value as human-readable text for diffs.
fn render_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(render_value)
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(map) => {
            if let Some(text) = map.get("t").and_then(Value::as_str) {
                return text.to_owned();
            }
            if let Some(encoded) = map.get("b64").and_then(Value::as_str)
                && let Ok(bytes) =
                    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded)
            {
                return format!(
                    "<{} binary bytes sha256:{:x}>",
                    bytes.len(),
                    sha2::Sha256::digest(&bytes)
                );
            }
            if let Some(digest) = map.get("blob").and_then(Value::as_str) {
                return format!("<blob {digest}>");
            }
            if let Some(id) = map.get("h") {
                return format!("<handle {id}>");
            }
            value.to_string()
        }
        _ => value.to_string(),
    }
}

/// Minimal unified diff: trims the common prefix/suffix of lines and shows
/// the differing middle as one hunk.
fn unified_diff(from: &str, to: &str, expected: &str, actual: &str) -> String {
    let mut out = format!("--- {from}\n+++ {to}\n");
    let expected_lines: Vec<&str> = expected.lines().collect();
    let actual_lines: Vec<&str> = actual.lines().collect();
    let mut prefix = 0;
    while prefix < expected_lines.len()
        && prefix < actual_lines.len()
        && expected_lines[prefix] == actual_lines[prefix]
    {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < expected_lines.len() - prefix
        && suffix < actual_lines.len() - prefix
        && expected_lines[expected_lines.len() - 1 - suffix]
            == actual_lines[actual_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }
    // Show one context line on each side when available.
    let context_before = prefix.min(1);
    let context_after = suffix.min(1);
    let old_start = prefix - context_before;
    let new_start = prefix - context_before;
    let old_end = expected_lines.len() - suffix + context_after;
    let new_end = actual_lines.len() - suffix + context_after;
    out.push_str(&format!(
        "@@ -{},{} +{},{} @@\n",
        old_start + 1,
        old_end - old_start,
        new_start + 1,
        new_end - new_start,
    ));
    for line in &expected_lines[old_start..prefix] {
        out.push_str(&format!(" {line}\n"));
    }
    for line in &expected_lines[prefix..expected_lines.len() - suffix] {
        out.push_str(&format!("-{line}\n"));
    }
    for line in &actual_lines[prefix..actual_lines.len() - suffix] {
        out.push_str(&format!("+{line}\n"));
    }
    for line in &expected_lines[expected_lines.len() - suffix..old_end] {
        out.push_str(&format!(" {line}\n"));
    }
    out
}

/// Handle the `exit` call: record it in record mode, replay the recorded
/// exit code in replay modes. Always diverges.
pub fn do_exit(code: i32) -> ! {
    let outcome = {
        let mut engine = lock_engine();
        match engine.mode {
            Mode::Passthrough => None,
            Mode::Record => {
                let record = engine.record.as_mut().expect("record state");
                let seq = record.next_seq;
                record.next_seq += 1;
                let line = json!({
                    "seq": seq,
                    "fn": "exit",
                    "args": [code as f64],
                    "ok": Value::Null,
                });
                let mut text = line.to_string();
                text.push('\n');
                if let Err(error) = record.writer.write_all(text.as_bytes()) {
                    fatal(&format!(
                        "cannot write tape {}: {error}",
                        record.path.display()
                    ));
                }
                record.bytes_written += text.len() as u64;
                write_footer(record);
                None
            }
            Mode::Replay => {
                let replay = engine.replay.as_mut().expect("replay state");
                // Exit replays the recorded exit code. The recorded code is
                // authoritative even if the program passes a different one:
                // pop an exact match first, else the first remaining entry.
                let key = key_for("exit", &json!([code as f64]));
                let index = replay.queues.get_mut(&key).and_then(VecDeque::pop_front);
                let index = index.or_else(|| {
                    let found = replay
                        .entries
                        .iter()
                        .enumerate()
                        .find(|(_, entry)| !entry.consumed && entry.func == "exit")
                        .map(|(index, entry)| (index, entry.func.clone(), entry.args.clone()));
                    found.map(|(index, sibling_func, sibling_args)| {
                        let sibling_key = key_for(&sibling_func, &sibling_args);
                        if let Some(queue) = replay.queues.get_mut(&sibling_key) {
                            queue.retain(|queued| *queued != index);
                        }
                        index
                    })
                });
                match index {
                    Some(index) => {
                        let entry = &mut replay.entries[index];
                        entry.consumed = true;
                        let recorded =
                            entry.args.get(0).and_then(Value::as_f64).unwrap_or(0.0) as i32;
                        if replay.diff {
                            Some(finalize_diff(replay, Some(recorded)))
                        } else {
                            // Borrow ends before exit; flush stderr for the report.
                            let _ = std::io::stderr().flush();
                            exit(recorded);
                        }
                    }
                    None => {
                        let report = mismatch_report(replay, "exit", &json!([code as f64]));
                        fatal(&report);
                    }
                }
            }
        }
    };
    match outcome {
        Some(final_code) => exit(final_code),
        None => {
            // Real exit path (pass-through and record modes), preserving the
            // upstream I/O flush hack. Called from async wrappers, so blocking
            // here briefly is acceptable.
            std::thread::sleep(std::time::Duration::from_millis(50));
            exit(code);
        }
    }
}

/// Flush record state and print the final replay report. Returns the process
/// exit code: 0 normally, 1 when diff mode found differences. Called by the
/// application template after `main` returns and by the `exit` wrapper.
pub fn finalize() -> i32 {
    let mut engine = lock_engine();
    match engine.mode {
        Mode::Passthrough => 0,
        Mode::Record => {
            let record = engine.record.as_mut().expect("record state");
            write_footer(record);
            0
        }
        Mode::Replay => {
            let replay = engine.replay.as_mut().expect("replay state");
            if replay.diff {
                finalize_diff(replay, None)
            } else {
                let remaining = replay
                    .entries
                    .iter()
                    .filter(|entry| !entry.consumed)
                    .count();
                if remaining > 0 {
                    eprintln!(
                        "os-tape: warning: {} of {} tape entries never consumed ({})",
                        remaining,
                        replay.entries.len(),
                        replay.path.display()
                    );
                }
                0
            }
        }
    }
}

/// Print the accumulated diff report plus missing expected outputs.
/// `exit_code` overrides the success code when set (replayed exit calls).
fn finalize_diff(replay: &mut ReplayState, exit_code: Option<i32>) -> i32 {
    for entry in replay.entries.iter().filter(|entry| !entry.consumed) {
        if is_output_fn(&entry.func) {
            replay.diffs.push(format!(
                "--- missing output: `{}` (tape seq={}) never produced\n--- expected\n{}",
                entry.func,
                entry.seq,
                render_value(&entry.args)
                    .lines()
                    .map(|line| format!("-{line}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ));
            replay.diff_failed = true;
        }
    }
    if replay.diff_failed {
        eprintln!(
            "os-tape: [PEN_TAPE=replay:{}] outputs differ from tape {} ({} difference{}):",
            replay.path.display(),
            replay.path.display(),
            replay.diffs.len(),
            if replay.diffs.len() == 1 { "" } else { "s" },
        );
        for diff in &replay.diffs {
            eprintln!("{diff}");
        }
        1
    } else {
        // Clean diff mode is silent: no news is good news, and `os-tape
        // check` prints its own per-tape verdicts.
        exit_code.unwrap_or(0)
    }
}

/// C entry point called by the application template before `main` runs, so
/// startup failures (a bad `PEN_TAPE` value, an unwritable tape path, a
/// missing replay tape) abort before the program itself starts.
#[unsafe(no_mangle)]
pub extern "C" fn _pen_os_tape_init() {
    // Force engine initialization now; the guard drops immediately.
    drop(lock_engine());
}

/// C entry point called by the application template after `main` returns.
#[unsafe(no_mangle)]
pub extern "C" fn _pen_os_tape_finalize() -> i32 {
    finalize()
}

/// Whether the engine is replaying in output-diff mode.
pub fn is_diff_replay() -> bool {
    let engine = lock_engine();
    engine.mode == Mode::Replay && engine.replay.as_ref().is_some_and(|replay| replay.diff)
}

/// Unwrap a tape decode result; a malformed tape is fatal.
pub fn decode<T>(result: Result<T, String>) -> T {
    result.unwrap_or_else(|message| fatal(&message))
}

/// Rebuild the error a program would have seen from a recorded message.
pub fn replay_error(message: String) -> Box<dyn std::error::Error> {
    Box::new(crate::error::OsError::Other(message))
}

/// Drain a Pen string list into owned byte strings. The list can only be
/// consumed once; wrappers rebuild it with `Vec::into` afterwards.
pub async fn drain_list(list: ffi::List) -> Vec<ffi::ByteString> {
    use futures::StreamExt;
    let items = ffi::future::stream::from_list(list);
    futures::pin_mut!(items);
    let mut owned = Vec::new();
    while let Some(item) = items.next().await {
        owned.push(ffi::ByteString::try_from(item).expect("tape: list element is not a string"));
    }
    owned
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn fn_table_covers_all_32_foreign_functions() {
        assert_eq!(FN_NAMES.len(), 32);
        for name in [
            "read_stdin",
            "read_limit_stdin",
            "write_stdout",
            "write_stderr",
            "open_file",
            "read_file",
            "read_limit_file",
            "write_file",
            "copy_file",
            "move_file",
            "remove_file",
            "read_directory",
            "create_directory",
            "remove_directory",
            "get_arguments",
            "get_environment_variable",
            "read_metadata",
            "tcp_bind",
            "tcp_connect",
            "tcp_accept",
            "tcp_receive",
            "tcp_send",
            "udp_bind",
            "udp_connect",
            "udp_receive",
            "udp_receive_from",
            "udp_send",
            "udp_send_to",
            "get_time",
            "sleep",
            "exit",
            "run_command",
        ] {
            assert!(is_known_fn(name), "missing {name}");
        }
    }

    #[test]
    fn output_classification_matches_prd() {
        for name in [
            "write_stdout",
            "write_stderr",
            "write_file",
            "copy_file",
            "move_file",
            "remove_file",
            "create_directory",
            "remove_directory",
            "tcp_send",
            "udp_send",
            "udp_send_to",
        ] {
            assert!(is_output_fn(name), "{name} should be an output call");
        }
        for name in [
            "read_stdin",
            "read_file",
            "read_directory",
            "get_arguments",
            "get_environment_variable",
            "get_time",
            "open_file",
            "tcp_accept",
            "udp_receive_from",
            "sleep",
            "exit",
            "run_command",
        ] {
            assert!(!is_output_fn(name), "{name} should be an input call");
        }
    }

    #[test]
    fn byte_encoding_round_trips() {
        // Valid UTF-8 is stored as text.
        assert_eq!(enc_bytes(b"hello\n"), json!({ "t": "hello\n" }));
        // Anything else is stored as base64.
        let raw = vec![0xff, 0xfe, 0x00, 0x01];
        let encoded = enc_bytes(&raw);
        assert_eq!(
            encoded,
            json!({ "b64": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &raw) })
        );
        for value in [json!({ "t": "hello\n" }), encoded] {
            let expected = dec_bytes(&value).expect("decode");
            assert_eq!(
                expected,
                if value.get("t").is_some() {
                    b"hello\n".to_vec()
                } else {
                    raw.clone()
                }
            );
        }
        // Handles and numbers round-trip.
        assert_eq!(dec_handle(&enc_handle(3)).expect("handle"), 3);
        assert_eq!(dec_number(&json!(41.0)).expect("number"), 41.0);
        assert!(dec_bytes(&json!(1)).is_err());
        assert!(dec_handle(&json!({ "t": "x" })).is_err());
    }

    #[test]
    fn per_key_queues_match_fifo() {
        let mut queues: HashMap<String, VecDeque<usize>> = HashMap::new();
        let key_a = key_for("read_file", &json!([{ "h": 1 }]));
        let key_b = key_for("read_file", &json!([{ "h": 2 }]));
        queues.entry(key_a.clone()).or_default().extend([0, 2]);
        queues.entry(key_b.clone()).or_default().extend([1]);
        // Independent keys pop independently of global order.
        assert_eq!(
            queues.get_mut(&key_b).and_then(VecDeque::pop_front),
            Some(1)
        );
        assert_eq!(
            queues.get_mut(&key_a).and_then(VecDeque::pop_front),
            Some(0)
        );
        assert_eq!(
            queues.get_mut(&key_a).and_then(VecDeque::pop_front),
            Some(2)
        );
        assert_eq!(queues.get_mut(&key_a).and_then(VecDeque::pop_front), None);
    }

    #[test]
    fn unified_diff_reports_exactly_the_changed_line() {
        let diff = unified_diff("tape", "actual", "one\nTWO\nthree\n", "one\ntwo\nthree\n");
        assert!(diff.contains("-TWO\n"), "diff:\n{diff}");
        assert!(diff.contains("+two\n"), "diff:\n{diff}");
        assert!(!diff.contains("-one"), "diff:\n{diff}");
        assert!(!diff.contains("-three"), "diff:\n{diff}");
        assert!(
            diff.starts_with("--- tape\n+++ actual\n@@ "),
            "diff:\n{diff}"
        );
        // Exactly one removed and one added content line besides the
        // `---`/`+++` file headers.
        let removed: Vec<&str> = diff
            .lines()
            .filter(|line| line.starts_with('-') && !line.starts_with("---"))
            .collect();
        let added: Vec<&str> = diff
            .lines()
            .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
            .collect();
        assert_eq!(removed, vec!["-TWO"], "diff:\n{diff}");
        assert_eq!(added, vec!["+two"], "diff:\n{diff}");
    }

    #[test]
    fn tape_versions_require_the_same_major() {
        assert_eq!(
            check_version(Some("0.1.0"), "0.1.0"),
            VersionCheck::Compatible
        );
        assert_eq!(
            check_version(Some("0.2.0"), "0.1.0"),
            VersionCheck::Compatible
        );
        assert_eq!(
            check_version(Some("1.0.0"), "0.1.0"),
            VersionCheck::Incompatible {
                recorded: "1.0.0".to_owned(),
                current: "0.1.0".to_owned(),
            }
        );
        assert_eq!(
            check_version(Some("0.1.0"), "2.0.0"),
            VersionCheck::Incompatible {
                recorded: "0.1.0".to_owned(),
                current: "2.0.0".to_owned(),
            }
        );
        // Missing or unparseable recorded versions fail open with a warning.
        assert_eq!(check_version(None, "0.1.0"), VersionCheck::Unknown);
        assert_eq!(
            check_version(Some("not-a-version"), "0.1.0"),
            VersionCheck::Unknown
        );
    }

    fn test_entry(seq: u64, func: &str, args: Value) -> Entry {
        Entry {
            seq,
            func: func.to_owned(),
            args,
            result: Ok(Value::Null),
            consumed: false,
        }
    }

    fn test_replay(entries: Vec<Entry>) -> ReplayState {
        ReplayState {
            path: PathBuf::from("test.tape"),
            blobs_dir: PathBuf::from("blobs"),
            entries,
            queues: HashMap::new(),
            handle_paths: HashMap::new(),
            diff: false,
            diffs: Vec::new(),
            diff_failed: false,
        }
    }

    /// A replay state whose per-key queues are populated from its entries, as
    /// `open_replay` would build them.
    fn test_replay_queued(entries: Vec<Entry>, diff: bool) -> ReplayState {
        let mut replay = test_replay(entries);
        replay.diff = diff;
        for (index, entry) in replay.entries.iter().enumerate() {
            replay
                .queues
                .entry(key_for(&entry.func, &entry.args))
                .or_default()
                .push_back(index);
        }
        replay
    }

    #[test]
    fn mismatch_report_names_call_and_nearest_entries() {
        let replay = test_replay(vec![
            test_entry(1, "open_file", json!([{ "t": "a.txt" }])),
            test_entry(2, "read_file", json!([{ "h": 1 }])),
            test_entry(3, "read_file", json!([{ "h": 2 }])),
            test_entry(4, "write_stdout", json!([{ "t": "hi" }])),
        ]);
        let report = mismatch_report(&replay, "read_file", &json!([{ "h": 9 }]));
        assert!(report.contains("read_file"), "report:\n{report}");
        assert!(report.contains(r#"{"h":9}"#), "report:\n{report}");
        assert!(
            report.contains("closest remaining entries"),
            "report:\n{report}"
        );
        // Same-function entries are preferred over other functions.
        assert!(report.contains("seq=2"), "report:\n{report}");
        assert!(report.contains("seq=3"), "report:\n{report}");
        assert!(!report.contains("seq=4"), "report:\n{report}");
    }

    #[test]
    fn mismatch_report_shows_at_most_three_entries() {
        let entries: Vec<Entry> = (1..=6)
            .map(|seq| test_entry(seq, "read_file", json!([{ "h": seq }])))
            .collect();
        let replay = test_replay(entries);
        let report = mismatch_report(&replay, "read_file", &json!([{ "h": 99 }]));
        assert!(report.contains("seq=1"), "report:\n{report}");
        assert!(report.contains("seq=2"), "report:\n{report}");
        assert!(report.contains("seq=3"), "report:\n{report}");
        assert!(!report.contains("seq=4"), "report:\n{report}");
        assert!(!report.contains("seq=5"), "report:\n{report}");
        // A call with no same-function entries left still sees the nearest
        // entries of other functions, at most three.
        let report = mismatch_report(&replay, "tcp_send", &json!([]));
        assert!(report.contains("nearest entries"), "report:\n{report}");
        assert!(report.contains("seq=1"), "report:\n{report}");
        assert!(report.contains("seq=3"), "report:\n{report}");
        assert!(!report.contains("seq=4"), "report:\n{report}");
    }

    #[test]
    fn values_render_readably() {
        assert_eq!(render_value(&json!({ "t": "hi" })), "hi");
        assert_eq!(render_value(&json!({ "h": 3 })), "<handle 3>");
        assert_eq!(render_value(&json!([{ "t": "a" }, { "t": "b" }])), "a\nb");
        assert_eq!(render_value(&json!(12.0)), "12.0");
        assert!(render_value(&json!({ "b64": "AAE=" })).contains("binary bytes"));
    }

    #[test]
    fn hypothetical_results_keep_diff_replay_going() {
        assert_eq!(
            hypothetical_ok("write_stdout", &json!([{ "t": "hello" }])),
            json!(5.0)
        );
        assert_eq!(
            hypothetical_ok("tcp_send", &json!([{ "h": 1 }, { "t": "hey" }])),
            json!(3.0)
        );
        assert_eq!(hypothetical_ok("copy_file", &json!([])), Value::Null);
    }

    #[test]
    fn default_size_cap_is_512_mib() {
        assert_eq!(DEFAULT_MAX_BYTES, 512 * 1024 * 1024);
        assert_eq!(human_bytes(DEFAULT_MAX_BYTES), "512 MiB");
        assert_eq!(human_bytes(1024), "1 KiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3 GiB");
        assert_eq!(human_bytes(1000), "1000 bytes");
        let message = size_cap_message(Path::new("run.tape"), DEFAULT_MAX_BYTES);
        assert!(message.contains("run.tape"), "message:\n{message}");
        assert!(message.contains("536870912"), "message:\n{message}");
        assert!(message.contains("512 MiB"), "message:\n{message}");
        assert!(
            message.contains("PEN_TAPE_MAX_BYTES"),
            "message:\n{message}"
        );
    }

    #[test]
    fn max_bytes_values_parse_with_suffixes() {
        assert_eq!(parse_max_bytes("1048576"), Ok(1048576));
        assert_eq!(parse_max_bytes("1M"), Ok(1024 * 1024));
        assert_eq!(parse_max_bytes("1m"), Ok(1024 * 1024));
        assert_eq!(parse_max_bytes("512MB"), Ok(512 * 1024 * 1024));
        assert_eq!(parse_max_bytes("2K"), Ok(2048));
        assert_eq!(parse_max_bytes("3kb"), Ok(3 * 1024));
        assert_eq!(parse_max_bytes("2G"), Ok(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_max_bytes(" 4T "), Ok(4 * 1024 * 1024 * 1024 * 1024));
        assert_eq!(parse_max_bytes("7B"), Ok(7));
        for bad in ["", "   ", "frobnicate", "1X", "M", "-5", "1.5M"] {
            let error = parse_max_bytes(bad).expect_err(&format!("{bad:?} should not parse"));
            assert!(error.contains("PEN_TAPE_MAX_BYTES"), "error:\n{error}");
            assert!(error.contains(bad.trim()), "error:\n{error}");
        }
        assert!(parse_max_bytes("18446744073709551615T").is_err());
    }

    #[test]
    fn diff_sleep_ignores_a_changed_duration() {
        let mut replay = test_replay_queued(vec![test_entry(1, "sleep", json!([10000.0]))], true);
        let result = diff_sleep_take(&mut replay, &json!([100.0]));
        assert_eq!(result, Ok(Value::Null));
        assert!(replay.entries[0].consumed);
        assert!(!replay.diff_failed);
        assert!(replay.diffs.is_empty());
        // An exact duration still matches exactly.
        let mut replay = test_replay_queued(vec![test_entry(1, "sleep", json!([50.0]))], true);
        assert_eq!(
            diff_sleep_take(&mut replay, &json!([50.0])),
            Ok(Value::Null)
        );
        assert!(replay.entries[0].consumed);
        // A sleep with nothing left on tape is timing only, so it passes.
        let mut replay = test_replay_queued(vec![], true);
        assert_eq!(
            diff_sleep_take(&mut replay, &json!([50.0])),
            Ok(Value::Null)
        );
        assert!(!replay.diff_failed);
    }

    #[test]
    fn diff_extra_output_names_the_unexpected_call() {
        let mut replay = test_replay_queued(vec![], true);
        let result = diff_output_take(&mut replay, "write_stdout", &json!([{ "t": "hi\n" }]));
        assert_eq!(result, Ok(json!(3.0)));
        assert!(replay.diff_failed);
        assert_eq!(replay.diffs.len(), 1);
        assert!(
            replay.diffs[0].contains("extra output"),
            "diff:\n{}",
            replay.diffs[0]
        );
        assert!(
            replay.diffs[0].contains("write_stdout"),
            "diff:\n{}",
            replay.diffs[0]
        );
    }

    #[test]
    fn diff_write_file_reports_the_path_and_content_diff() {
        let mut replay = test_replay_queued(
            vec![test_entry(
                3,
                "write_file",
                json!([{ "h": 1 }, { "t": "line1\n" }]),
            )],
            true,
        );
        replay
            .handle_paths
            .insert(1, "/tmp/work/out.txt".to_owned());
        let result = diff_output_take(
            &mut replay,
            "write_file",
            &json!([{ "h": 1 }, { "t": "line1\nline2\n" }]),
        );
        assert_eq!(result, Ok(Value::Null));
        assert!(replay.diff_failed);
        assert_eq!(replay.diffs.len(), 1);
        let diff = &replay.diffs[0];
        assert!(diff.contains("/tmp/work/out.txt"), "diff:\n{diff}");
        assert!(diff.contains("@@ "), "diff:\n{diff}");
        assert!(diff.contains("+line2\n"), "diff:\n{diff}");
        assert!(!diff.contains("-line1"), "diff:\n{diff}");
    }

    #[test]
    fn byte_values_decode_to_text_without_the_lock() {
        assert_eq!(
            bytes_to_text(&json!({ "t": "a.txt" })),
            Some("a.txt".to_owned())
        );
        assert_eq!(
            bytes_to_text(&json!({ "b64": "YQ==" })),
            Some("a".to_owned())
        );
        assert_eq!(bytes_to_text(&json!({ "blob": "e".repeat(64) })), None);
        assert_eq!(bytes_to_text(&json!(1)), None);
    }

    #[test]
    fn fn_table_matches_context_pen_foreign_functions() {
        // The wrapper table must cover exactly the foreign functions declared
        // in Context/context.pen: parse them the way drift-check.sh does.
        let context = include_str!("../../../Context/context.pen");
        let mut parsed = std::collections::BTreeSet::new();
        for chunk in context.split("_pen_os_").skip(1) {
            let name: String = chunk
                .chars()
                .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_')
                .collect();
            if !name.is_empty() {
                parsed.insert(name);
            }
        }
        let table: std::collections::BTreeSet<String> =
            FN_NAMES.iter().map(|name| name.to_string()).collect();
        assert_eq!(
            parsed, table,
            "tape wrapper table drifted from Context/context.pen"
        );
        assert_eq!(table.len(), 32);
    }

    #[test]
    fn replay_take_succeeds_for_8_interleaved_threads() {
        use std::sync::{Arc, Barrier, Mutex};
        // Eight independent reads served through one shared replay state from
        // eight threads released at once: every thread must get its entry.
        let replay = Arc::new(Mutex::new(test_replay_queued(
            (1..=8u64)
                .map(|handle| test_entry(handle, "read_file", json!([{ "h": handle }])))
                .collect(),
            false,
        )));
        let barrier = Arc::new(Barrier::new(8));
        std::thread::scope(|scope| {
            for handle in 1..=8u64 {
                let replay = Arc::clone(&replay);
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    barrier.wait();
                    let taken = replay
                        .lock()
                        .expect("replay lock")
                        .take("read_file", &json!([{ "h": handle }]));
                    assert!(
                        taken.is_ok(),
                        "thread for handle {handle} failed: {taken:?}"
                    );
                });
            }
        });
        let replay = replay.lock().expect("replay lock");
        assert_eq!(replay.entries.len(), 8);
        assert!(
            replay.entries.iter().all(|entry| entry.consumed),
            "every entry must be consumed exactly once"
        );
    }

    /// Lexicographic next permutation; false when `order` is already last.
    fn next_permutation(order: &mut [u64]) -> bool {
        let Some(pivot) = (0..order.len() - 1)
            .rev()
            .find(|&index| order[index] < order[index + 1])
        else {
            return false;
        };
        let swap = (pivot + 1..order.len())
            .rev()
            .find(|&index| order[index] > order[pivot])
            .expect("a successor exists past the pivot");
        order.swap(pivot, swap);
        order[pivot + 1..].reverse();
        true
    }

    #[test]
    fn replay_matching_succeeds_in_every_arrival_order() {
        // Eight distinct keys, one entry each: concurrent tasks can interleave
        // arbitrarily, so all 8! arrival orders must replay.
        let mut order: Vec<u64> = (1..=8).collect();
        let mut checked = 0u32;
        loop {
            let mut replay = test_replay_queued(
                (1..=8u64)
                    .map(|handle| test_entry(handle, "read_file", json!([{ "h": handle }])))
                    .collect(),
                false,
            );
            for handle in &order {
                let taken = replay.take("read_file", &json!([{ "h": *handle }]));
                assert!(taken.is_ok(), "order {order:?} failed at handle {handle}");
            }
            assert!(replay.entries.iter().all(|entry| entry.consumed));
            checked += 1;
            if !next_permutation(&mut order) {
                break;
            }
        }
        assert_eq!(checked, 40320);
    }

    #[test]
    fn mismatch_report_fits_40_lines_and_ends_with_a_suggestion() {
        let replay = test_replay(vec![test_entry(1, "open_file", json!([{ "t": "a.txt" }]))]);
        let report = mismatch_report(&replay, "read_file", &json!([{ "h": 9 }]));
        assert!(report.contains("mode=replay"), "report:\n{report}");
        assert!(report.contains("test.tape"), "report:\n{report}");
        let lines: Vec<&str> = report.lines().collect();
        assert!(lines.len() <= 40, "report:\n{report}");
        assert!(
            lines
                .last()
                .expect("report has lines")
                .contains("re-record with os-tape record"),
            "report:\n{report}"
        );
        // Even megabyte arguments stay within budget on one truncated line.
        let huge = "x".repeat(2 * 1024 * 1024);
        let report = mismatch_report(&replay, "write_stdout", &json!([{ "t": huge }]));
        let lines: Vec<&str> = report.lines().collect();
        assert!(lines.len() <= 40, "{} lines", lines.len());
        assert!(
            lines
                .last()
                .expect("report has lines")
                .contains("re-record with os-tape record")
        );
        assert!(
            report.contains("truncated"),
            "report should note truncation"
        );
    }

    #[test]
    fn blob_digests_must_be_64_lowercase_hex() {
        assert!(is_valid_blob_digest(
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        ));
        let upper = "e".repeat(64).to_uppercase();
        let short = "e".repeat(63);
        let non_hex = "g".repeat(64);
        for bad in [
            "",
            "../evil",
            "blobs/evil",
            upper.as_str(),
            short.as_str(),
            non_hex.as_str(),
        ] {
            assert!(!is_valid_blob_digest(bad), "{bad:?} should be rejected");
        }
        // Decoding rejects a traversal digest before touching the filesystem,
        // even outside replay mode.
        let error = dec_bytes(&json!({ "blob": "../evil" })).expect_err("traversal must fail");
        assert!(error.contains("bad blob digest"), "error:\n{error}");
        assert!(error.contains("../evil"), "error:\n{error}");
    }

    #[test]
    fn blob_spill_threshold_is_64_kib() {
        assert_eq!(BLOB_THRESHOLD, 64 * 1024);
    }

    #[test]
    fn blob_digests_are_lowercase_hex_sha256() {
        assert_eq!(
            blob_digest(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(is_valid_blob_digest(&blob_digest(&vec![7u8; 70000])));
        // Pass-through mode (unit tests run with PEN_TAPE unset) keeps large
        // values inline: nothing is matched or stored there.
        let inline = enc_bytes(&vec![0xffu8; 70000]);
        assert!(
            inline.get("b64").is_some(),
            "expected inline base64, got {inline}"
        );
    }
}
