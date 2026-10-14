use crate::{error::OsError, tape, utilities};
use std::error::Error;
use tokio::fs;

#[ffi::bindgen]
#[ffi::runtime]
async fn _pen_os_read_directory(path: ffi::ByteString) -> Result<ffi::List, Box<dyn Error>> {
    let args = serde_json::json!([tape::enc_bytes(path.as_slice())]);
    match tape::replay_take("read_directory", &args) {
        tape::ReplayOutcome::Passthrough => {
            let result = real_read_directory(path).await;
            match result {
                Ok(list) => {
                    // Drain the list to serialize it, then rebuild it.
                    let owned = tape::drain_list(list).await;
                    let raw: Vec<Vec<u8>> =
                        owned.iter().map(|item| item.as_slice().to_vec()).collect();
                    tape::record("read_directory", args, Ok(tape::enc_str_list(&raw)));
                    Ok(owned.into())
                }
                Err(error) => {
                    tape::record("read_directory", args, Err(error.to_string()));
                    Err(error)
                }
            }
        }
        tape::ReplayOutcome::Return(outcome) => match outcome {
            Ok(value) => {
                let items = tape::decode(tape::dec_str_list(&value));
                Ok(items
                    .into_iter()
                    .map(ffi::ByteString::from)
                    .collect::<Vec<_>>()
                    .into())
            }
            Err(message) => Err(tape::replay_error(message)),
        },
    }
}

async fn real_read_directory(path: ffi::ByteString) -> Result<ffi::List, Box<dyn Error>> {
    let mut read_dir = fs::read_dir(utilities::decode_path(&path)?).await?;
    let mut entries = vec![];

    while let Some(entry) = read_dir.next_entry().await? {
        entries.push(ffi::ByteString::from(
            entry
                .file_name()
                .into_string()
                .map_err(|_| OsError::Other("cannot decode path".into()))?,
        ));
    }

    Ok(entries.into())
}

#[ffi::bindgen]
#[ffi::runtime]
async fn _pen_os_create_directory(path: ffi::ByteString) -> Result<(), Box<dyn Error>> {
    let args = serde_json::json!([tape::enc_bytes(path.as_slice())]);
    match tape::replay_take("create_directory", &args) {
        tape::ReplayOutcome::Passthrough => {
            let result: Result<(), Box<dyn Error>> = fs::create_dir(utilities::decode_path(&path)?)
                .await
                .map_err(|error| error.into());
            tape::record(
                "create_directory",
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

#[ffi::bindgen]
#[ffi::runtime]
async fn _pen_os_remove_directory(path: ffi::ByteString) -> Result<(), Box<dyn Error>> {
    let args = serde_json::json!([tape::enc_bytes(path.as_slice())]);
    match tape::replay_take("remove_directory", &args) {
        tape::ReplayOutcome::Passthrough => {
            let result: Result<(), Box<dyn Error>> = fs::remove_dir(utilities::decode_path(&path)?)
                .await
                .map_err(|error| error.into());
            tape::record(
                "remove_directory",
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
