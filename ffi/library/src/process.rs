use crate::tape;
use std::{error::Error, str};

#[ffi::bindgen]
#[ffi::runtime]
async fn _pen_os_exit(code: ffi::Number) -> ffi::None {
    tape::do_exit(f64::from(code) as i32)
}

#[ffi::bindgen]
#[ffi::runtime]
async fn _pen_os_run_command(
    command: ffi::ByteString,
    arguments: ffi::List,
) -> Result<(), Box<dyn Error>> {
    // Drain the argument list first: it can only be consumed once, so the
    // real call gets a rebuilt copy.
    let owned = tape::drain_list(arguments).await;
    let raw: Vec<Vec<u8>> = owned.iter().map(|item| item.as_slice().to_vec()).collect();
    let args = serde_json::json!([
        tape::enc_bytes(command.as_slice()),
        tape::enc_str_list(&raw),
    ]);
    match tape::replay_take("run_command", &args) {
        tape::ReplayOutcome::Passthrough => {
            let rebuilt: ffi::List = owned.into();
            let result = real_run_command(command, rebuilt).await;
            tape::record(
                "run_command",
                args,
                result
                    .as_ref()
                    .map(|_| serde_json::Value::Null)
                    .map_err(|error| error.to_string()),
            );
            result
        }
        tape::ReplayOutcome::Return(outcome) => match outcome {
            Ok(_) => Ok(()),
            Err(message) => Err(tape::replay_error(message)),
        },
    }
}

async fn real_run_command(
    command: ffi::ByteString,
    arguments: ffi::List,
) -> Result<(), Box<dyn Error>> {
    use futures::{StreamExt, pin_mut};

    let arguments = ffi::future::stream::from_list(arguments);

    pin_mut!(arguments);

    let mut args = vec![];

    while let Some(argument) = arguments.next().await {
        args.push(ffi::ByteString::try_from(argument).unwrap());
    }

    tokio::process::Command::new(str::from_utf8(command.as_slice())?)
        .args(
            args.iter()
                .map(|string| str::from_utf8(string.as_slice()))
                .collect::<Result<Vec<_>, _>>()?,
        )
        .output()
        .await?;

    Ok(())
}
