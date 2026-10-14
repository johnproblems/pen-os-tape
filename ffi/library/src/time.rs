use crate::tape;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::sleep;

#[ffi::bindgen]
fn _pen_os_get_time() -> ffi::Number {
    let args = serde_json::json!([]);
    match tape::replay_take("get_time", &args) {
        tape::ReplayOutcome::Passthrough => {
            let time = real_get_time();
            tape::record("get_time", args, Ok(serde_json::json!(f64::from(time))));
            time
        }
        tape::ReplayOutcome::Return(outcome) => ffi::Number::new(tape::decode(
            outcome.and_then(|value| tape::dec_number(&value)),
        )),
    }
}

fn real_get_time() -> ffi::Number {
    (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|error| error.duration())
        .as_millis() as f64)
        .into()
}

#[ffi::bindgen]
#[ffi::runtime]
async fn _pen_os_sleep(milliseconds: ffi::Number) {
    let args = serde_json::json!([f64::from(milliseconds)]);
    match tape::replay_take("sleep", &args) {
        tape::ReplayOutcome::Passthrough => {
            real_sleep(milliseconds).await;
            tape::record("sleep", args, Ok(serde_json::Value::Null));
        }
        // Replay returns immediately: no sleeping on the tape.
        tape::ReplayOutcome::Return(_) => {}
    }
}

async fn real_sleep(milliseconds: ffi::Number) {
    sleep(Duration::from_millis(f64::from(milliseconds) as u64)).await;
}
