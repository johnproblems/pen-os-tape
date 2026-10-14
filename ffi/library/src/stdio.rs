use crate::{tape, utilities};
use core::ops::DerefMut;
use std::{error::Error, sync::LazyLock};
use tokio::{
    io::{AsyncWriteExt, Stderr, Stdin, Stdout, stderr, stdin, stdout},
    sync::Mutex,
};

// Use single buffers for standard I/O.
static STDIN: LazyLock<Mutex<Stdin>> = LazyLock::new(|| Mutex::new(stdin()));
static STDOUT: LazyLock<Mutex<Stdout>> = LazyLock::new(|| Mutex::new(stdout()));
static STDERR: LazyLock<Mutex<Stderr>> = LazyLock::new(|| Mutex::new(stderr()));

#[ffi::bindgen]
#[ffi::runtime]
async fn _pen_os_read_stdin() -> Result<ffi::ByteString, Box<dyn Error>> {
    let args = serde_json::json!([]);
    match tape::replay_take("read_stdin", &args) {
        tape::ReplayOutcome::Passthrough => {
            let result = utilities::read(&mut STDIN.lock().await.deref_mut()).await;
            tape::record(
                "read_stdin",
                args,
                result
                    .as_ref()
                    .map(|bytes| tape::enc_bytes(bytes.as_slice()))
                    .map_err(|error| error.to_string()),
            );
            result
        }
        tape::ReplayOutcome::Return(outcome) => match outcome {
            Ok(value) => Ok(tape::decode(tape::dec_bytes(&value)).into()),
            Err(message) => Err(tape::replay_error(message)),
        },
    }
}

#[ffi::bindgen]
#[ffi::runtime]
async fn _pen_os_read_limit_stdin(limit: ffi::Number) -> Result<ffi::ByteString, Box<dyn Error>> {
    let args = serde_json::json!([f64::from(limit)]);
    match tape::replay_take("read_limit_stdin", &args) {
        tape::ReplayOutcome::Passthrough => {
            let result = utilities::read_limit(
                &mut STDIN.lock().await.deref_mut(),
                f64::from(limit) as usize,
            )
            .await;
            tape::record(
                "read_limit_stdin",
                args,
                result
                    .as_ref()
                    .map(|bytes| tape::enc_bytes(bytes.as_slice()))
                    .map_err(|error| error.to_string()),
            );
            result
        }
        tape::ReplayOutcome::Return(outcome) => match outcome {
            Ok(value) => Ok(tape::decode(tape::dec_bytes(&value)).into()),
            Err(message) => Err(tape::replay_error(message)),
        },
    }
}

#[ffi::bindgen]
#[ffi::runtime]
async fn _pen_os_write_stdout(bytes: ffi::ByteString) -> Result<ffi::Number, Box<dyn Error>> {
    let args = serde_json::json!([tape::enc_bytes(bytes.as_slice())]);
    match tape::replay_take("write_stdout", &args) {
        tape::ReplayOutcome::Passthrough => {
            let mut stdout = STDOUT.lock().await;
            let result = utilities::write(&mut *stdout, bytes).await;
            if result.is_ok()
                && let Err(error) = stdout.flush().await
            {
                let error: Box<dyn Error> = error.into();
                tape::record("write_stdout", args, Err(error.to_string()));
                return Err(error);
            }
            tape::record(
                "write_stdout",
                args,
                result
                    .as_ref()
                    .map(|count| serde_json::json!(f64::from(*count)))
                    .map_err(|error| error.to_string()),
            );
            result
        }
        tape::ReplayOutcome::Return(outcome) => match outcome {
            Ok(value) => {
                let count = tape::decode(tape::dec_number(&value));
                if !tape::is_diff_replay() {
                    // Strict replay re-emits the matched payload so the
                    // replayed run's stdout is byte-identical. (Diff mode
                    // compares instead of printing; disk and network outputs
                    // are never re-performed in any replay mode.)
                    let bytes = tape::decode(tape::dec_bytes(&args[0]));
                    let mut stdout = STDOUT.lock().await;
                    if stdout.write_all(&bytes).await.is_ok() {
                        let _ = stdout.flush().await;
                    }
                }
                Ok(ffi::Number::new(count))
            }
            Err(message) => Err(tape::replay_error(message)),
        },
    }
}

#[ffi::bindgen]
#[ffi::runtime]
async fn _pen_os_write_stderr(bytes: ffi::ByteString) -> Result<ffi::Number, Box<dyn Error>> {
    let args = serde_json::json!([tape::enc_bytes(bytes.as_slice())]);
    match tape::replay_take("write_stderr", &args) {
        tape::ReplayOutcome::Passthrough => {
            let mut stderr = STDERR.lock().await;
            let result = utilities::write(&mut *stderr, bytes).await;
            if result.is_ok()
                && let Err(error) = stderr.flush().await
            {
                let error: Box<dyn Error> = error.into();
                tape::record("write_stderr", args, Err(error.to_string()));
                return Err(error);
            }
            tape::record(
                "write_stderr",
                args,
                result
                    .as_ref()
                    .map(|count| serde_json::json!(f64::from(*count)))
                    .map_err(|error| error.to_string()),
            );
            result
        }
        tape::ReplayOutcome::Return(outcome) => match outcome {
            Ok(value) => {
                let count = tape::decode(tape::dec_number(&value));
                if !tape::is_diff_replay() {
                    let bytes = tape::decode(tape::dec_bytes(&args[0]));
                    let mut stderr = STDERR.lock().await;
                    if stderr.write_all(&bytes).await.is_ok() {
                        let _ = stderr.flush().await;
                    }
                }
                Ok(ffi::Number::new(count))
            }
            Err(message) => Err(tape::replay_error(message)),
        },
    }
}
