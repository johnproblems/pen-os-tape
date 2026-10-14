use crate::tape;
use std::{error::Error, str, sync::Arc};
use tokio::{
    net,
    sync::{RwLock, RwLockWriteGuard},
};

const MAX_UDP_PAYLOAD_SIZE: usize = 512;

#[ffi::into_any(into_fn = "_pen_os_udp_socket_to_any")]
#[repr(C)]
#[derive(Clone)]
pub struct UdpSocket(ffi::Arc<ffi::Any>);

// See file.rs: the `any` payload fits one pointer, so the id and the optional
// live socket live behind an `Arc`.
#[ffi::any]
#[derive(Clone)]
struct UdpSocketInner(Arc<UdpSocketHandle>);

struct UdpSocketHandle {
    id: u32,
    socket: Option<RwLock<net::UdpSocket>>,
}

impl UdpSocket {
    pub fn live(socket: net::UdpSocket, id: u32) -> Self {
        Self(ffi::Arc::new(
            UdpSocketInner(Arc::new(UdpSocketHandle {
                id,
                socket: Some(RwLock::new(socket)),
            }))
            .into(),
        ))
    }

    pub fn replay(id: u32) -> Self {
        Self(ffi::Arc::new(
            UdpSocketInner(Arc::new(UdpSocketHandle { id, socket: None })).into(),
        ))
    }

    pub fn tape_id(&self) -> u32 {
        TryInto::<&UdpSocketInner>::try_into(&*self.0).unwrap().0.id
    }

    pub async fn lock(&self) -> RwLockWriteGuard<'_, net::UdpSocket> {
        TryInto::<&UdpSocketInner>::try_into(&*self.0)
            .unwrap()
            .0
            .socket
            .as_ref()
            .expect("os-tape: replay stand-in UDP socket has no live handle")
            .write()
            .await
    }
}

#[ffi::into_any(into_fn = "_pen_os_udp_datagram_to_any")]
#[repr(C)]
#[derive(Clone, Debug)]
pub struct UdpDatagram(ffi::Arc<UdpDatagramInner>);

#[repr(C)]
#[derive(Clone, Debug)]
struct UdpDatagramInner {
    data: ffi::ByteString,
    address: ffi::ByteString,
}

impl UdpDatagram {
    pub fn new(data: ffi::ByteString, address: ffi::ByteString) -> Self {
        Self(UdpDatagramInner { data, address }.into())
    }

    pub fn data(&self) -> &ffi::ByteString {
        &self.0.data
    }

    pub fn address(&self) -> &ffi::ByteString {
        &self.0.address
    }
}

#[ffi::bindgen]
#[ffi::runtime]
async fn _pen_os_udp_bind(address: ffi::ByteString) -> Result<UdpSocket, Box<dyn Error>> {
    let args = serde_json::json!([tape::enc_bytes(address.as_slice())]);
    match tape::replay_take("udp_bind", &args) {
        tape::ReplayOutcome::Passthrough => {
            let result: Result<net::UdpSocket, Box<dyn Error>> =
                net::UdpSocket::bind(str::from_utf8(address.as_slice())?)
                    .await
                    .map_err(|error| error.into());
            match result {
                Ok(socket) => {
                    let id = tape::alloc_handle();
                    tape::record("udp_bind", args, Ok(tape::enc_handle(id)));
                    Ok(UdpSocket::live(socket, id))
                }
                Err(error) => {
                    tape::record("udp_bind", args, Err(error.to_string()));
                    Err(error)
                }
            }
        }
        tape::ReplayOutcome::Return(outcome) => match outcome {
            Ok(value) => Ok(UdpSocket::replay(tape::decode(tape::dec_handle(&value)))),
            Err(message) => Err(tape::replay_error(message)),
        },
    }
}

#[ffi::bindgen]
#[ffi::runtime]
async fn _pen_os_udp_connect(
    socket: UdpSocket,
    address: ffi::ByteString,
) -> Result<(), Box<dyn Error>> {
    let args = serde_json::json!([
        tape::enc_handle(socket.tape_id()),
        tape::enc_bytes(address.as_slice()),
    ]);
    match tape::replay_take("udp_connect", &args) {
        tape::ReplayOutcome::Passthrough => {
            let result: Result<(), Box<dyn Error>> = socket
                .lock()
                .await
                .connect(str::from_utf8(address.as_slice())?)
                .await
                .map_err(|error| error.into());
            tape::record(
                "udp_connect",
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
async fn _pen_os_udp_receive(socket: UdpSocket) -> Result<ffi::ByteString, Box<dyn Error>> {
    let args = serde_json::json!([tape::enc_handle(socket.tape_id())]);
    match tape::replay_take("udp_receive", &args) {
        tape::ReplayOutcome::Passthrough => {
            let result = real_udp_receive(socket).await;
            tape::record(
                "udp_receive",
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

async fn real_udp_receive(socket: UdpSocket) -> Result<ffi::ByteString, Box<dyn Error>> {
    let mut buffer = vec![0; MAX_UDP_PAYLOAD_SIZE];
    let size = socket.lock().await.recv(&mut buffer).await?;

    buffer.truncate(size);

    Ok(buffer.into())
}

#[ffi::bindgen]
#[ffi::runtime]
async fn _pen_os_udp_receive_from(socket: UdpSocket) -> Result<UdpDatagram, Box<dyn Error>> {
    let args = serde_json::json!([tape::enc_handle(socket.tape_id())]);
    match tape::replay_take("udp_receive_from", &args) {
        tape::ReplayOutcome::Passthrough => {
            let result = real_udp_receive_from(socket).await;
            tape::record(
                "udp_receive_from",
                args,
                result
                    .as_ref()
                    .map(|datagram| {
                        serde_json::json!({
                            "data": tape::enc_bytes(datagram.data().as_slice()),
                            "address": tape::enc_bytes(datagram.address().as_slice()),
                        })
                    })
                    .map_err(|error| error.to_string()),
            );
            result
        }
        tape::ReplayOutcome::Return(outcome) => {
            match outcome {
                Ok(value) => {
                    let data =
                        tape::decode(value.get("data").map(tape::dec_bytes).unwrap_or_else(|| {
                            Err(format!("bad datagram value in tape: {value}"))
                        }));
                    let address =
                        tape::decode(value.get("address").map(tape::dec_bytes).unwrap_or_else(
                            || Err(format!("bad datagram value in tape: {value}")),
                        ));
                    Ok(UdpDatagram::new(data.into(), address.into()))
                }
                Err(message) => Err(tape::replay_error(message)),
            }
        }
    }
}

async fn real_udp_receive_from(socket: UdpSocket) -> Result<UdpDatagram, Box<dyn Error>> {
    let mut buffer = vec![0; MAX_UDP_PAYLOAD_SIZE];
    let (size, address) = socket.lock().await.recv_from(&mut buffer).await?;

    buffer.truncate(size);

    Ok(UdpDatagram::new(buffer.into(), address.to_string().into()))
}

#[ffi::bindgen]
#[ffi::runtime]
async fn _pen_os_udp_send(
    socket: UdpSocket,
    data: ffi::ByteString,
) -> Result<ffi::Number, Box<dyn Error>> {
    let args = serde_json::json!([
        tape::enc_handle(socket.tape_id()),
        tape::enc_bytes(data.as_slice()),
    ]);
    match tape::replay_take("udp_send", &args) {
        tape::ReplayOutcome::Passthrough => {
            let result: Result<ffi::Number, Box<dyn Error>> = socket
                .lock()
                .await
                .send(data.as_slice())
                .await
                .map(|size| (size as f64).into())
                .map_err(|error| error.into());
            tape::record(
                "udp_send",
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
async fn _pen_os_udp_send_to(
    socket: UdpSocket,
    data: ffi::ByteString,
    address: ffi::ByteString,
) -> Result<ffi::Number, Box<dyn Error>> {
    let args = serde_json::json!([
        tape::enc_handle(socket.tape_id()),
        tape::enc_bytes(data.as_slice()),
        tape::enc_bytes(address.as_slice()),
    ]);
    match tape::replay_take("udp_send_to", &args) {
        tape::ReplayOutcome::Passthrough => {
            let result: Result<ffi::Number, Box<dyn Error>> = socket
                .lock()
                .await
                .send_to(data.as_slice(), str::from_utf8(address.as_slice())?)
                .await
                .map(|size| (size as f64).into())
                .map_err(|error| error.into());
            tape::record(
                "udp_send_to",
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
