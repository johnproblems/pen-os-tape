use crate::tape;

#[ffi::bindgen]
fn _pen_os_get_arguments() -> ffi::List {
    let args = serde_json::json!([]);
    match tape::replay_take("get_arguments", &args) {
        tape::ReplayOutcome::Passthrough => {
            let list = real_get_arguments();
            // Re-read argv for the tape: draining the ffi list needs async.
            let raw: Vec<Vec<u8>> = std::env::args()
                .skip(1)
                .map(|arg| arg.into_bytes())
                .collect();
            tape::record("get_arguments", args, Ok(tape::enc_str_list(&raw)));
            list
        }
        tape::ReplayOutcome::Return(outcome) => {
            let items = tape::decode(outcome.and_then(|value| tape::dec_str_list(&value)));
            items
                .into_iter()
                .map(ffi::ByteString::from)
                .collect::<Vec<_>>()
                .into()
        }
    }
}

fn real_get_arguments() -> ffi::List {
    std::env::args()
        .skip(1)
        .map(ffi::ByteString::from)
        .collect::<Vec<_>>()
        .into()
}
