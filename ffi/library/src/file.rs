use super::open_file_options::OpenFileOptions;
use crate::{tape, utilities};
use std::{error::Error, ops::DerefMut, path::Path, sync::Arc};
use tokio::{
    fs,
    sync::{RwLock, RwLockWriteGuard},
};

#[ffi::into_any(into_fn = "_pen_os_file_to_any")]
#[repr(C)]
#[derive(Clone)]
struct File(ffi::Arc<ffi::Any>);

// The `#[ffi::any]` payload fits one pointer, so the handle state lives
// behind an `Arc`: a tape handle id plus the live file when one exists.
// Clones share the `Arc`, so copies of a handle keep the same id.
#[ffi::any]
#[derive(Clone)]
struct FileInner(Arc<FileHandle>);

struct FileHandle {
    id: u32,
    // `None` for replay stand-ins, which are never used for live I/O.
    file: Option<RwLock<fs::File>>,
}

impl File {
    pub fn live(file: fs::File, id: u32) -> Self {
        Self(ffi::Arc::new(
            FileInner(Arc::new(FileHandle {
                id,
                file: Some(RwLock::new(file)),
            }))
            .into(),
        ))
    }

    pub fn replay(id: u32) -> Self {
        Self(ffi::Arc::new(
            FileInner(Arc::new(FileHandle { id, file: None })).into(),
        ))
    }

    pub fn tape_id(&self) -> u32 {
        TryInto::<&FileInner>::try_into(&*self.0).unwrap().0.id
    }

    pub async fn lock(&self) -> RwLockWriteGuard<'_, fs::File> {
        TryInto::<&FileInner>::try_into(&*self.0)
            .unwrap()
            .0
            .file
            .as_ref()
            .expect("os-tape: replay stand-in file has no live handle")
            .write()
            .await
    }
}

#[ffi::into_any(into_fn = "_pen_os_file_metadata_to_any")]
#[repr(C)]
struct FileMetadata(ffi::Arc<FileMetadataInner>);

#[repr(C)]
struct FileMetadataInner {
    size: ffi::Number,
}

impl FileMetadata {
    pub fn new(size: ffi::Number) -> Self {
        Self(FileMetadataInner { size }.into())
    }

    pub fn size(&self) -> f64 {
        f64::from(self.0.size)
    }
}

#[ffi::bindgen]
#[ffi::runtime]
async fn _pen_os_open_file(
    path: ffi::ByteString,
    options: OpenFileOptions,
) -> Result<File, Box<dyn Error>> {
    let (append, create, create_new, read, truncate, write) = options.tape_flags();
    let args = serde_json::json!([
        tape::enc_bytes(path.as_slice()),
        {
            "opts": {
                "append": append,
                "create": create,
                "create_new": create_new,
                "read": read,
                "truncate": truncate,
                "write": write,
            }
        },
    ]);
    match tape::replay_take("open_file", &args) {
        tape::ReplayOutcome::Passthrough => {
            let result = real_open_file(path, options).await;
            match result {
                Ok(file) => {
                    let id = tape::alloc_handle();
                    tape::record("open_file", args, Ok(tape::enc_handle(id)));
                    Ok(File::live(file, id))
                }
                Err(error) => {
                    tape::record("open_file", args, Err(error.to_string()));
                    Err(error)
                }
            }
        }
        tape::ReplayOutcome::Return(outcome) => match outcome {
            Ok(value) => Ok(File::replay(tape::decode(tape::dec_handle(&value)))),
            Err(message) => Err(tape::replay_error(message)),
        },
    }
}

async fn real_open_file(
    path: ffi::ByteString,
    options: OpenFileOptions,
) -> Result<fs::File, Box<dyn Error>> {
    Ok(fs::OpenOptions::from(options)
        .open(&Path::new(&utilities::decode_path(&path)?))
        .await?)
}

#[ffi::bindgen]
#[ffi::runtime]
async fn _pen_os_read_file(file: File) -> Result<ffi::ByteString, Box<dyn Error>> {
    let args = serde_json::json!([tape::enc_handle(file.tape_id())]);
    match tape::replay_take("read_file", &args) {
        tape::ReplayOutcome::Passthrough => {
            let result = utilities::read(file.lock().await.deref_mut()).await;
            tape::record(
                "read_file",
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
async fn _pen_os_read_limit_file(
    file: File,
    limit: ffi::Number,
) -> Result<ffi::ByteString, Box<dyn Error>> {
    let args = serde_json::json!([tape::enc_handle(file.tape_id()), f64::from(limit)]);
    match tape::replay_take("read_limit_file", &args) {
        tape::ReplayOutcome::Passthrough => {
            let result =
                utilities::read_limit(file.lock().await.deref_mut(), f64::from(limit) as usize)
                    .await;
            tape::record(
                "read_limit_file",
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
async fn _pen_os_write_file(
    file: File,
    bytes: ffi::ByteString,
) -> Result<ffi::Number, Box<dyn Error>> {
    let args = serde_json::json!([
        tape::enc_handle(file.tape_id()),
        tape::enc_bytes(bytes.as_slice()),
    ]);
    match tape::replay_take("write_file", &args) {
        tape::ReplayOutcome::Passthrough => {
            let result = utilities::write(file.lock().await.deref_mut(), bytes).await;
            tape::record(
                "write_file",
                args,
                result
                    .as_ref()
                    .map(|count| serde_json::json!(f64::from(*count)))
                    .map_err(|error| error.to_string()),
            );
            result
        }
        tape::ReplayOutcome::Return(outcome) => match outcome {
            Ok(value) => Ok(ffi::Number::new(tape::decode(tape::dec_number(&value)))),
            Err(message) => Err(tape::replay_error(message)),
        },
    }
}

#[ffi::bindgen]
#[ffi::runtime]
async fn _pen_os_copy_file(
    src: ffi::ByteString,
    dest: ffi::ByteString,
) -> Result<(), Box<dyn Error>> {
    let args = serde_json::json!([
        tape::enc_bytes(src.as_slice()),
        tape::enc_bytes(dest.as_slice()),
    ]);
    match tape::replay_take("copy_file", &args) {
        tape::ReplayOutcome::Passthrough => {
            let result: Result<(), Box<dyn Error>> = fs::copy(
                utilities::decode_path(&src)?,
                utilities::decode_path(&dest)?,
            )
            .await
            .map(|_| ())
            .map_err(|error| error.into());
            tape::record(
                "copy_file",
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
async fn _pen_os_move_file(
    src: ffi::ByteString,
    dest: ffi::ByteString,
) -> Result<(), Box<dyn Error>> {
    let args = serde_json::json!([
        tape::enc_bytes(src.as_slice()),
        tape::enc_bytes(dest.as_slice()),
    ]);
    match tape::replay_take("move_file", &args) {
        tape::ReplayOutcome::Passthrough => {
            let result: Result<(), Box<dyn Error>> = fs::rename(
                utilities::decode_path(&src)?,
                utilities::decode_path(&dest)?,
            )
            .await
            .map_err(|error| error.into());
            tape::record(
                "move_file",
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
async fn _pen_os_remove_file(path: ffi::ByteString) -> Result<(), Box<dyn Error>> {
    let args = serde_json::json!([tape::enc_bytes(path.as_slice())]);
    match tape::replay_take("remove_file", &args) {
        tape::ReplayOutcome::Passthrough => {
            let result: Result<(), Box<dyn Error>> =
                fs::remove_file(utilities::decode_path(&path)?)
                    .await
                    .map_err(|error| error.into());
            tape::record(
                "remove_file",
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
async fn _pen_os_read_metadata(path: ffi::ByteString) -> Result<FileMetadata, Box<dyn Error>> {
    let args = serde_json::json!([tape::enc_bytes(path.as_slice())]);
    match tape::replay_take("read_metadata", &args) {
        tape::ReplayOutcome::Passthrough => {
            let result = real_read_metadata(path).await;
            tape::record(
                "read_metadata",
                args,
                result
                    .as_ref()
                    .map(|metadata| serde_json::json!({ "size": metadata.size() }))
                    .map_err(|error| error.to_string()),
            );
            result
        }
        tape::ReplayOutcome::Return(outcome) => {
            match outcome {
                Ok(value) => {
                    let size =
                        tape::decode(value.get("size").map(tape::dec_number).unwrap_or_else(
                            || Err(format!("bad metadata value in tape: {value}")),
                        ));
                    Ok(FileMetadata::new(ffi::Number::new(size)))
                }
                Err(message) => Err(tape::replay_error(message)),
            }
        }
    }
}

async fn real_read_metadata(path: ffi::ByteString) -> Result<FileMetadata, Box<dyn Error>> {
    let metadata = fs::metadata(utilities::decode_path(&path)?).await?;

    Ok(FileMetadata::new((metadata.len() as f64).into()))
}
