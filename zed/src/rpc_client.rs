use anyhow::{anyhow, Result};
use futures::future::Either;
use postage::{
    barrier, mpsc, oneshot,
    prelude::{Sink, Stream},
};
use smol::{
    io::BoxedWriter,
    lock::{Mutex, RwLock},
    prelude::{AsyncRead, AsyncWrite},
};
use std::{
    any::TypeId,
    collections::{HashMap, HashSet},
    future::Future,
    sync::{
        atomic::{self, AtomicU32},
        Arc,
    },
};
use zed_rpc::proto::{self, EnvelopedMessage, MessageStream, RequestMessage};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ConnectionId(u32);

struct RpcConnection {
    writer: Mutex<MessageStream<BoxedWriter>>,
    response_channels: Mutex<HashMap<u32, oneshot::Sender<proto::Envelope>>>,
    next_message_id: AtomicU32,
    _close_barrier: barrier::Sender,
}

type MessageHandler =
    Box<dyn Send + Sync + Fn(&mut Option<proto::Envelope>, ConnectionId) -> Option<ErasedMessage>>;

struct ErasedMessage {
    id: u32,
    connection_id: ConnectionId,
    body: proto::Envelope,
}

pub struct Message<T: EnvelopedMessage> {
    connection_id: ConnectionId,
    body: Option<T>,
}

impl<T: EnvelopedMessage> From<ErasedMessage> for Message<T> {
    fn from(message: ErasedMessage) -> Self {
        Self {
            connection_id: message.connection_id,
            body: T::from_envelope(message.body),
        }
    }
}

impl<T: EnvelopedMessage> Message<T> {
    pub fn connection_id(&self) -> ConnectionId {
        self.connection_id
    }

    pub fn body(&mut self) -> T {
        self.body.take().expect("body already taken")
    }
}

pub struct Request<T: RequestMessage> {
    id: u32,
    connection_id: ConnectionId,
    body: Option<T>,
}

impl<T: RequestMessage> From<ErasedMessage> for Request<T> {
    fn from(message: ErasedMessage) -> Self {
        Self {
            id: message.id,
            connection_id: message.connection_id,
            body: T::from_envelope(message.body),
        }
    }
}

impl<T: RequestMessage> Request<T> {
    pub fn connection_id(&self) -> ConnectionId {
        self.connection_id
    }

    pub fn body(&mut self) -> T {
        self.body.take().expect("body already taken")
    }
}

pub struct RpcClient {
    connections: RwLock<HashMap<ConnectionId, Arc<RpcConnection>>>,
    message_handlers: RwLock<Vec<(mpsc::Sender<ErasedMessage>, MessageHandler)>>,
    handler_types: Mutex<HashSet<TypeId>>,
    next_connection_id: AtomicU32,
}

impl RpcClient {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            connections: Default::default(),
            message_handlers: Default::default(),
            handler_types: Default::default(),
            next_connection_id: Default::default(),
        })
    }

    pub async fn add_request_handler<T: RequestMessage>(&self) -> impl Stream<Item = Request<T>> {
        if !self.handler_types.lock().await.insert(TypeId::of::<T>()) {
            panic!("duplicate handler type");
        }

        let (tx, rx) = mpsc::channel(256);
        self.message_handlers.write().await.push((
            tx,
            Box::new(move |envelope, connection_id| {
                if envelope.as_ref().map_or(false, T::matches_envelope) {
                    let envelope = Option::take(envelope).unwrap();
                    Some(ErasedMessage {
                        id: envelope.id,
                        connection_id,
                        body: envelope,
                    })
                } else {
                    None
                }
            }),
        ));
        rx.map(Request::from)
    }

    pub async fn add_message_handler<T: EnvelopedMessage>(&self) -> impl Stream<Item = Message<T>> {
        if !self.handler_types.lock().await.insert(TypeId::of::<T>()) {
            panic!("duplicate handler type");
        }

        let (tx, rx) = mpsc::channel(256);
        self.message_handlers.write().await.push((
            tx,
            Box::new(move |envelope, connection_id| {
                if envelope.as_ref().map_or(false, T::matches_envelope) {
                    let envelope = Option::take(envelope).unwrap();
                    Some(ErasedMessage {
                        id: envelope.id,
                        connection_id,
                        body: envelope,
                    })
                } else {
                    None
                }
            }),
        ));
        rx.map(Message::from)
    }

    pub async fn add_connection<Conn>(
        self: &Arc<Self>,
        conn: Conn,
    ) -> (ConnectionId, impl Future<Output = ()>)
    where
        Conn: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let connection_id = ConnectionId(
            self.next_connection_id
                .fetch_add(1, atomic::Ordering::SeqCst),
        );
        let (close_tx, mut close_rx) = barrier::channel();
        let (conn_rx, conn_tx) = smol::io::split(conn);
        let connection = Arc::new(RpcConnection {
            writer: Mutex::new(MessageStream::new(Box::pin(conn_tx))),
            response_channels: Default::default(),
            next_message_id: Default::default(),
            _close_barrier: close_tx,
        });

        self.connections
            .write()
            .await
            .insert(connection_id, connection.clone());

        let this = self.clone();
        let handler_future = async move {
            let closed = close_rx.recv();
            smol::pin!(closed);

            let mut stream = MessageStream::new(conn_rx);
            loop {
                let read_message = stream.read_message();
                smol::pin!(read_message);

                match futures::future::select(read_message, &mut closed).await {
                    Either::Left((Ok(incoming), _)) => {
                        if let Some(responding_to) = incoming.responding_to {
                            let channel = connection
                                .response_channels
                                .lock()
                                .await
                                .remove(&responding_to);
                            if let Some(mut tx) = channel {
                                tx.send(incoming).await.ok();
                            } else {
                                log::warn!(
                                    "received RPC response to unknown request {}",
                                    responding_to
                                );
                            }
                        } else {
                            let mut handled = false;
                            let mut envelope = Some(incoming);
                            for (tx, handler) in this.message_handlers.read().await.iter() {
                                if let Some(message) = handler(&mut envelope, connection_id) {
                                    let _ = tx.clone().send(message).await;
                                    handled = true;
                                    break;
                                }
                            }

                            if !handled {
                                log::warn!("unhandled message: {:?}", envelope.unwrap().payload);
                            }
                        }
                    }
                    Either::Left((Err(error), _)) => {
                        log::warn!("received invalid RPC message: {}", error);
                    }
                    Either::Right(_) => break,
                }
            }
        };

        (connection_id, handler_future)
    }

    pub async fn disconnect(&self, connection_id: ConnectionId) {
        self.connections.write().await.remove(&connection_id);
    }

    pub fn request<T: RequestMessage>(
        self: &Arc<Self>,
        connection_id: ConnectionId,
        req: T,
    ) -> impl Future<Output = Result<T::Response>> {
        let this = self.clone();
        let (tx, mut rx) = oneshot::channel();
        async move {
            let connection = this
                .connections
                .read()
                .await
                .get(&connection_id)
                .ok_or_else(|| anyhow!("unknown connection: {}", connection_id.0))?
                .clone();
            let message_id = connection
                .next_message_id
                .fetch_add(1, atomic::Ordering::SeqCst);
            connection
                .response_channels
                .lock()
                .await
                .insert(message_id, tx);
            connection
                .writer
                .lock()
                .await
                .write_message(&req.into_envelope(message_id, None))
                .await?;
            let response = rx
                .recv()
                .await
                .expect("response channel was unexpectedly dropped");
            T::Response::from_envelope(response)
                .ok_or_else(|| anyhow!("received response of the wrong type"))
        }
    }

    pub fn send<T: EnvelopedMessage>(
        self: &Arc<Self>,
        connection_id: ConnectionId,
        message: T,
    ) -> impl Future<Output = Result<()>> {
        let this = self.clone();
        async move {
            let connection = this
                .connections
                .read()
                .await
                .get(&connection_id)
                .ok_or_else(|| anyhow!("unknown connection: {}", connection_id.0))?
                .clone();
            let message_id = connection
                .next_message_id
                .fetch_add(1, atomic::Ordering::SeqCst);
            connection
                .writer
                .lock()
                .await
                .write_message(&message.into_envelope(message_id, None))
                .await?;
            Ok(())
        }
    }

    pub fn respond<T: RequestMessage>(
        self: &Arc<Self>,
        request: Request<T>,
        response: T::Response,
    ) -> impl Future<Output = Result<()>> {
        let this = self.clone();
        async move {
            let connection = this
                .connections
                .read()
                .await
                .get(&request.connection_id)
                .ok_or_else(|| anyhow!("unknown connection: {}", request.connection_id.0))?
                .clone();
            let message_id = connection
                .next_message_id
                .fetch_add(1, atomic::Ordering::SeqCst);
            connection
                .writer
                .lock()
                .await
                .write_message(&response.into_envelope(message_id, Some(request.id)))
                .await?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smol::{
        future::poll_once,
        io::AsyncWriteExt,
        net::unix::{UnixListener, UnixStream},
    };
    use std::{future::Future, io};
    use tempdir::TempDir;

    #[gpui::test]
    async fn test_request_response(cx: gpui::TestAppContext) {
        let executor = cx.read(|app| app.background_executor().clone());
        let socket_dir_path = TempDir::new("request-response").unwrap();
        let socket_path = socket_dir_path.path().join(".sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let client_conn = UnixStream::connect(&socket_path).await.unwrap();
        let (server_conn, _) = listener.accept().await.unwrap();

        let mut server_stream = MessageStream::new(server_conn);
        let client = RpcClient::new();
        let (connection_id, handler) = client.add_connection(client_conn).await;
        executor.spawn(handler).detach();

        let client_req = client.request(
            connection_id,
            proto::Auth {
                user_id: 42,
                access_token: "token".to_string(),
            },
        );
        smol::pin!(client_req);
        let server_req = send_recv(&mut client_req, server_stream.read_message())
            .await
            .unwrap();
        assert_eq!(
            server_req.payload,
            Some(proto::envelope::Payload::Auth(proto::Auth {
                user_id: 42,
                access_token: "token".to_string()
            }))
        );

        // Respond to another request to ensure requests are properly matched up.
        server_stream
            .write_message(
                &proto::AuthResponse {
                    credentials_valid: false,
                }
                .into_envelope(1000, Some(999)),
            )
            .await
            .unwrap();
        server_stream
            .write_message(
                &proto::AuthResponse {
                    credentials_valid: true,
                }
                .into_envelope(1001, Some(server_req.id)),
            )
            .await
            .unwrap();
        assert_eq!(
            client_req.await.unwrap(),
            proto::AuthResponse {
                credentials_valid: true
            }
        );
    }

    #[gpui::test]
    async fn test_disconnect(cx: gpui::TestAppContext) {
        let executor = cx.read(|app| app.background_executor().clone());
        let socket_dir_path = TempDir::new("drop-client").unwrap();
        let socket_path = socket_dir_path.path().join(".sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let client_conn = UnixStream::connect(&socket_path).await.unwrap();
        let (mut server_conn, _) = listener.accept().await.unwrap();

        let client = RpcClient::new();
        let (connection_id, handler) = client.add_connection(client_conn).await;
        executor.spawn(handler).detach();
        client.disconnect(connection_id).await;

        // Try sending an empty payload over and over, until the client is dropped and hangs up.
        loop {
            match server_conn.write(&[]).await {
                Ok(_) => {}
                Err(err) => {
                    if err.kind() == io::ErrorKind::BrokenPipe {
                        break;
                    }
                }
            }
        }
    }

    #[gpui::test]
    async fn test_io_error(cx: gpui::TestAppContext) {
        let executor = cx.read(|app| app.background_executor().clone());
        let socket_dir_path = TempDir::new("io-error").unwrap();
        let socket_path = socket_dir_path.path().join(".sock");
        let _listener = UnixListener::bind(&socket_path).unwrap();
        let mut client_conn = UnixStream::connect(&socket_path).await.unwrap();
        client_conn.close().await.unwrap();

        let client = RpcClient::new();
        let (connection_id, handler) = client.add_connection(client_conn).await;
        executor.spawn(handler).detach();
        let err = client
            .request(
                connection_id,
                proto::Auth {
                    user_id: 42,
                    access_token: "token".to_string(),
                },
            )
            .await
            .unwrap_err();
        assert_eq!(
            err.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    async fn send_recv<S, R, O>(mut sender: S, receiver: R) -> O
    where
        S: Unpin + Future,
        R: Future<Output = O>,
    {
        smol::pin!(receiver);
        loop {
            poll_once(&mut sender).await;
            match poll_once(&mut receiver).await {
                Some(message) => break message,
                None => continue,
            }
        }
    }
}
