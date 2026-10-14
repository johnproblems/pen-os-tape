use crate::tape;
use std::{error::Error, ffi::OsString, str};

#[ffi::bindgen]
fn _pen_os_get_environment_variable(
    name: ffi::ByteString,
) -> Result<ffi::ByteString, Box<dyn Error>> {
    let args = serde_json::json!([tape::enc_bytes(name.as_slice())]);
    match tape::replay_take("get_environment_variable", &args) {
        tape::ReplayOutcome::Passthrough => {
            let result = real_get_environment_variable(name);
            tape::record(
                "get_environment_variable",
                args,
                result
                    .as_ref()
                    .map(|value| tape::enc_bytes(value.as_slice()))
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

fn real_get_environment_variable(name: ffi::ByteString) -> Result<ffi::ByteString, Box<dyn Error>> {
    Ok(std::env::var(OsString::from(str::from_utf8(name.as_slice())?))?.into())
}
