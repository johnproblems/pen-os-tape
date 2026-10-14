use crate::tape;
use std::{error::Error, str, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net,
    sync::{RwLock, RwLockWriteGuard},
};

#[ffi::into_any(into_fn = "_pen_os_tcp_listener_to_any")]
#[repr(C)]
#[derive(Clone)]
struct TcpListener(ffi::Arc<ffi::Any>);

// See file.rs: the `any` payload fits one pointer, so the id and the optional
// live socket live behind an `Arc`.
#[ffi::any]
#[derive(Clone)]
struct TcpListenerInner(Arc<TcpListenerHandle>);

struct TcpListenerHandle {
    id: u32,
    listener: Option<RwLock<net::TcpListener>>,
}

impl TcpListener {
    pub fn live(listener: net::TcpListener, id: u32) -> Self {
        Self(ffi::Arc::new(
            TcpListenerInner(Arc::new(TcpListenerHandle {
                id,
                listener: Some(RwLock::new(listener)),
            }))
            .into(),
        ))
    }

    pub fn replay(id: u32) -> Self {
        Self(ffi::Arc::new(
            TcpListenerInner(Arc::new(TcpListenerHandle { id, listener: None })).into(),
        ))
    }

    pub fn tape_id(&self) -> u32 {
        TryInto::<&TcpListenerInner>::try_into(&*self.0)
            .unwrap()
            .0
            .id
    }

    pub async fn lock(&self) -> RwLockWriteGuard<'_, net::TcpListener> {
        TryInto::<&TcpListenerInner>::try_into(&*self.0)
            .unwrap()
            .0
            .listener
            .as_ref()
            .expect("os-tape: replay stand-in TCP listener has no live handle")
            .write()
            .await
    }
}

#[ffi::into_any(into_fn = "_pen_os_tcp_stream_to_any")]
#[repr(C)]
#[derive(Clone)]
struct TcpStream(ffi::Arc<ffi::Any>);

#[ffi::any]
#[derive(Clone)]
struct TcpStreamInner(Arc<TcpStreamHandle>);

struct TcpStreamHandle {
    id: u32,
    stream: Option<RwLock<net::TcpStream>>,
}

impl TcpStream {
    pub fn live(stream: net::TcpStream, id: u32) -> Self {
        Self(ffi::Arc::new(
            TcpStreamInner(Arc::new(TcpStreamHandle {
                id,
                stream: Some(RwLock::new(stream)),
            }))
            .into(),
        ))
    }

    pub fn replay(id: u32) -> Self {
        Self(ffi::Arc::new(
            TcpStreamInner(Arc::new(TcpStreamHandle { id, stream: None })).into(),
        ))
    }

    pub fn tape_id(&self) -> u32 {
        TryInto::<&TcpStreamInner>::try_into(&*self.0).unwrap().0.id
    }

    pub async fn lock(&self) -> RwLockWriteGuard<'_, net::TcpStream> {
        TryInto::<&TcpStreamInner>::try_into(&*self.0)
            .unwrap()
            .0
            .stream
            .as_ref()
            .expect("os-tape: replay stand-in TCP stream has no live handle")
            .write()
            .await
    }
}

#[ffi::into_any(into_fn = "_pen_os_tcp_accepted_stream_to_any")]
#[repr(C)]
#[derive(Clone)]
struct TcpAcceptedStream(ffi::Arc<TcpAcceptedStreamInner>);

#[repr(C)]
struct TcpAcceptedStreamInner {
    stream: TcpStream,
    address: ffi::ByteString,
}

impl TcpAcceptedStream {
    pub fn new(stream: TcpStream, address: ffi::ByteString) -> Self {
        Self(ffi::Arc::new(TcpAcceptedStreamInner { stream, address }))
    }
}

#[ffi::bindgen]
#[ffi::runtime]
async fn _pen_os_tcp_bind(address: ffi::ByteString) -> Result<TcpListener, Box<dyn Error>> {
    let args = serde_json::json!([tape::enc_bytes(address.as_slice())]);
    match tape::replay_take("tcp_bind", &args) {
        tape::ReplayOutcome::Passthrough => {
            let result: Result<net::TcpListener, Box<dyn Error>> =
                net::TcpListener::bind(str::from_utf8(address.as_slice())?)
                    .await
                    .map_err(|error| error.into());
            match result {
                Ok(listener) => {
                    let id = tape::alloc_handle();
                    tape::record("tcp_bind", args, Ok(tape::enc_handle(id)));
                    Ok(TcpListener::live(listener, id))
                }
                Err(error) => {
                    tape::record("tcp_bind", args, Err(error.to_string()));
                    Err(error)
                }
            }
        }
        tape::ReplayOutcome::Return(outcome) => match outcome {
            Ok(value) => Ok(TcpListener::replay(tape::decode(tape::dec_handle(&value)))),
            Err(message) => Err(tape::replay_error(message)),
        },
    }
}

#[ffi::bindgen]
#[ffi::runtime]
async fn _pen_os_tcp_connect(address: ffi::ByteString) -> Result<TcpStream, Box<dyn Error>> {
    let args = serde_json::json!([tape::enc_bytes(address.as_slice())]);
    match tape::replay_take("tcp_connect", &args) {
        tape::ReplayOutcome::Passthrough => {
            let result: Result<net::TcpStream, Box<dyn Error>> =
                net::TcpStream::connect(str::from_utf8(address.as_slice())?)
                    .await
                    .map_err(|error| error.into());
            match result {
                Ok(stream) => {
                    let id = tape::alloc_handle();
                    tape::record("tcp_connect", args, Ok(tape::enc_handle(id)));
                    Ok(TcpStream::live(stream, id))
                }
                Err(error) => {
                    tape::record("tcp_connect", args, Err(error.to_string()));
                    Err(error)
                }
            }
        }
        tape::ReplayOutcome::Return(outcome) => match outcome {
            Ok(value) => Ok(TcpStream::replay(tape::decode(tape::dec_handle(&value)))),
            Err(message) => Err(tape::replay_error(message)),
        },
    }
}

#[ffi::bindgen]
#[ffi::runtime]
async fn _pen_os_tcp_accept(listener: TcpListener) -> Result<TcpAcceptedStream, Box<dyn Error>> {
    let args = serde_json::json!([tape::enc_handle(listener.tape_id())]);
    match tape::replay_take("tcp_accept", &args) {
        tape::ReplayOutcome::Passthrough => {
            let result: Result<(net::TcpStream, String), Box<dyn Error>> = listener
                .lock()
                .await
                .accept()
                .await
                .map(|(stream, address)| (stream, address.to_string()))
                .map_err(|error| error.into());
            match result {
                Ok((stream, address)) => {
                    let id = tape::alloc_handle();
                    let address: ffi::ByteString = address.into();
                    tape::record(
                        "tcp_accept",
                        args,
                        Ok(serde_json::json!({
                            "stream": tape::enc_handle(id),
                            "addr": tape::enc_bytes(address.as_slice()),
                        })),
                    );
                    Ok(TcpAcceptedStream::new(TcpStream::live(stream, id), address))
                }
                Err(error) => {
                    tape::record("tcp_accept", args, Err(error.to_string()));
                    Err(error)
                }
            }
        }
        tape::ReplayOutcome::Return(outcome) => {
            match outcome {
                Ok(value) => {
                    let id =
                        tape::decode(value.get("stream").map(tape::dec_handle).unwrap_or_else(
                            || Err(format!("bad accepted-stream value in tape: {value}")),
                        ));
                    let address =
                        tape::decode(value.get("addr").map(tape::dec_bytes).unwrap_or_else(|| {
                            Err(format!("bad accepted-stream value in tape: {value}"))
                        }));
                    Ok(TcpAcceptedStream::new(
                        TcpStream::replay(id),
                        ffi::ByteString::from(address),
                    ))
                }
                Err(message) => Err(tape::replay_error(message)),
            }
        }
    }
}

#[ffi::bindgen]
#[ffi::runtime]
async fn _pen_os_tcp_receive(
    socket: TcpStream,
    limit: ffi::Number,
) -> Result<ffi::ByteString, Box<dyn Error>> {
    let args = serde_json::json!([tape::enc_handle(socket.tape_id()), f64::from(limit)]);
    match tape::replay_take("tcp_receive", &args) {
        tape::ReplayOutcome::Passthrough => {
            let result = real_tcp_receive(socket, limit).await;
            tape::record(
                "tcp_receive",
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

async fn real_tcp_receive(
    socket: TcpStream,
    limit: ffi::Number,
) -> Result<ffi::ByteString, Box<dyn Error>> {
    let mut buffer = vec![0; f64::from(limit) as usize];
    let size = socket.lock().await.read(&mut buffer).await?;

    buffer.truncate(size);

    Ok(buffer.into())
}

#[ffi::bindgen]
#[ffi::runtime]
async fn _pen_os_tcp_send(
    socket: TcpStream,
    data: ffi::ByteString,
) -> Result<ffi::Number, Box<dyn Error>> {
    let args = serde_json::json!([
        tape::enc_handle(socket.tape_id()),
        tape::enc_bytes(data.as_slice()),
    ]);
    match tape::replay_take("tcp_send", &args) {
        tape::ReplayOutcome::Passthrough => {
            let result: Result<ffi::Number, Box<dyn Error>> = socket
                .lock()
                .await
                .write(data.as_slice())
                .await
                .map(|size| (size as f64).into())
                .map_err(|error| error.into());
            tape::record(
                "tcp_send",
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
