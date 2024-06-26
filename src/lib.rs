#![cfg_attr(doc, feature(doc_cfg))]
use async_trait::async_trait;
use futures_util::{stream::StreamExt, SinkExt};
use prost::Message;
use std::{collections::HashMap, sync::Arc, time::Duration};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, oneshot, RwLock};

#[cfg(any(doc, feature = "builtin_connection_metadata"))]
#[cfg_attr(doc, doc(cfg(feature = "builtin_connection_metadata")))]
pub mod default_connection_metadata;
// #[cfg(any(doc, feature = "builtin_connection_metadata"))]
// pub use default_connection_metadata::DefaultConnectionMetadataFetcher;

pub mod goval;

#[derive(Error, Debug)]
pub enum CrosisError {
    #[error("Failed to fetch connection metadata")]
    ConnectionMetadataFetchError,
    #[error("Failed to parse connection metadata: {0}")]
    ConnectionMetadataParseError(String),
    #[error("Failed to connect to websocket: {0}")]
    ConnectionError(tokio_tungstenite::tungstenite::Error),
    #[error("Client isn't connected, cannot {0}")]
    Disconnected(String),
    #[error("Client got unexpected reply {0:#?}")]
    UnexpectedReply(goval::Command),
    #[error("Client got error message {0}")]
    GenericError(String),
}

#[derive(Debug)]
pub enum ConnectionStatus {
    // WebsocketExists {
    //     connecting: bool,
    //     write: SplitSink<
    //         tokio_tungstenite::WebSocketStream<
    //             tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    //         >,
    //         tokio_tungstenite::tungstenite::Message,
    //     >,
    //     // read: SplitStream<
    //     //     tokio_tungstenite::WebSocketStream<
    //     //         tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    //     //     >,
    //     // >,
    // },
    /// The client is currently connecting and not able to open channels
    Connecting,
    /// The client is fully connected and ready to open channels / send messages.
    Connected,
    /// The client is not connected to a websocket yet, or [Client::close](Client::close) was called.
    Disconnected,
}

type ReqMap = Arc<RwLock<HashMap<String, oneshot::Sender<goval::Command>>>>;

type ChanMap = Arc<RwLock<HashMap<i32, (mpsc::UnboundedSender<goval::Command>, ReqMap)>>>;

/// Represents a crosis client similar to <https://github.com/replit/crosis>.
#[readonly::make]
pub struct Client {
    /// Represents the current [ConnectionStatus](ConnectionStatus) of the client.
    /// Wrapped in an [`Arc<RwLock<T>>`] so the websocket task can read and write to it.
    #[readonly]
    pub status: Arc<RwLock<ConnectionStatus>>,

    /// If status is not disconnected this will always be `Some(T)`.
    /// Right now they aren't reset back to None when client is closed,
    /// so them existing doesn't mean the client is functioning. These
    /// aren't stored as part of the connection status enum because
    /// that's behind an `Arc<RwLock<T>>`.
    /// Once reconnects are handled these will likely be behind their
    /// own `Arc<RwLock<T>>`'s
    closer: Option<broadcast::Sender<()>>,

    /// If status is not disconnected this will always be `Some(T)`.
    /// Right now they aren't reset back to None when client is closed,
    /// so them existing doesn't mean the client is functioning. These
    /// aren't stored as part of the connection status enum because
    /// that's behind an `Arc<RwLock<T>>`.
    /// Once reconnects are handled these will likely be behind their
    /// own `Arc<RwLock<T>>`'s
    writer: Option<mpsc::UnboundedSender<Vec<u8>>>,

    channel_map: ChanMap,

    /// Uhhhhh, pain
    fetcher: Box<dyn ConnectionMetadataFetcher + Sync + Send>,
}

impl Client {
    pub fn new(fetcher: Box<dyn ConnectionMetadataFetcher + Send + Sync>) -> Client {
        Client {
            fetcher,
            closer: None,
            writer: None,
            channel_map: Arc::new(RwLock::new(HashMap::new())),
            status: Arc::new(RwLock::new(ConnectionStatus::Disconnected)),
        }
    }

    pub async fn connect(&mut self) -> Result<Channel, CrosisError> {
        let mut retries: u32 = 0;
        let connection_metadata = loop {
            break match self.fetcher.fetch().await {
                Ok(res) => res,
                Err(err) => match err {
                    FetchConnectionMetadataError::Abort => {
                        return Err(CrosisError::ConnectionMetadataFetchError);
                    }
                    FetchConnectionMetadataError::Other(err) => return Err(err),
                    FetchConnectionMetadataError::Retriable => {
                        if retries >= 3 {
                            return Err(CrosisError::ConnectionMetadataFetchError);
                        }
                        tokio::time::sleep(Duration::from_millis(10 * 10u64.pow(retries))).await;
                        retries += 1;
                        continue;
                    }
                },
            };
        };

        let mut conn_url = match url::Url::parse(&connection_metadata.gurl) {
            Ok(url) => url,
            Err(err) => {
                return Err(CrosisError::ConnectionMetadataParseError(err.to_string()));
            }
        };

        conn_url.set_path(&format!("/wsv2/{}", connection_metadata.token));

        let (connection, _resp) = match tokio_tungstenite::connect_async(conn_url).await {
            Ok(conn) => conn,
            Err(err) => return Err(CrosisError::ConnectionError(err)),
        };

        //
        // read.next().await;
        // read.reunite(write).unwrap().close(None);
        let (msgsend, msgrecv) = mpsc::unbounded_channel();
        let (sender, recv) = broadcast::channel(1);
        self.writer = Some(msgsend);
        self.closer = Some(sender.clone());

        let (chan0, chan0_sender) =
            Channel::new(0, self.status.clone(), self.writer.clone().unwrap());
        let mut lck = self.channel_map.write().await;
        lck.insert(0, (chan0_sender, chan0.req_map.clone()));
        drop(lck);

        tokio::spawn(bg_loop(
            sender,
            recv.resubscribe(),
            self.status.clone(),
            msgrecv,
            self.channel_map.clone(),
            connection,
        ));

        loop {
            if let ConnectionStatus::Connected = *self.status.read().await {
                break;
            }

            tokio::task::yield_now().await;
        }

        // println!("READY");
        Ok(chan0)
    }

    pub async fn close(&self) -> Result<(), CrosisError> {
        match *self.status.read().await {
            ConnectionStatus::Disconnected => {
                Err(CrosisError::Disconnected("send message".to_string()))
            }
            _ => {
                self.closer.as_ref().unwrap().send(()).unwrap();
                Ok(())
            }
        }
    }

    /// This method will not check that the client is fully connected. The user is responsible for making sure that the client is not still in the [Connecting](ConnectionStatus::Connecting) state.
    pub async fn send(&self, msg: goval::Command) -> Result<(), CrosisError> {
        match *self.status.read().await {
            ConnectionStatus::Disconnected => {
                Err(CrosisError::Disconnected("send message".to_string()))
            }
            _ => {
                // Writer wont be closed if client isnt disconnected
                self.writer
                    .as_ref()
                    .unwrap()
                    .send(msg.encode_to_vec())
                    .unwrap();
                Ok(())
            }
        }
    }

    /// Opens a new channel, action defaults to [`AttachOrCreate`](goval::open_channel::Action::AttachOrCreate)
    pub async fn open(
        &self,
        service: String,
        name: Option<String>,
        action: Option<goval::open_channel::Action>,
    ) -> Result<Channel, CrosisError> {
        match *self.status.read().await {
            ConnectionStatus::Connected => {}
            _ => {
                return Err(CrosisError::Disconnected("open channel".to_string()));
            }
        }

        let action = action.unwrap_or(goval::open_channel::Action::AttachOrCreate);
        let name = name.unwrap_or("".to_string());

        let cmd_ref = generate_ref();

        let open_cmd = goval::Command {
            channel: 0,
            r#ref: cmd_ref.clone(),
            body: Some(goval::command::Body::OpenChan(goval::OpenChannel {
                service,
                name,
                action: action.into(),
                ..Default::default()
            })),
            ..Default::default()
        };

        let cmap = self.channel_map.read().await;
        let mut reqs = cmap.get(&0).unwrap().1.write().await;

        let (send, recv) = oneshot::channel();
        reqs.insert(cmd_ref, send);
        drop(reqs);
        drop(cmap);

        self.send(open_cmd).await?;

        let resp = recv.await.unwrap();
        // println!("Fun");
        match resp.clone().body {
            Some(goval::command::Body::OpenChanRes(res)) => match res.state() {
                goval::open_channel_res::State::Error => Err(CrosisError::GenericError(res.error)),
                _ => {
                    let (channel, sender) =
                        Channel::new(res.id, self.status.clone(), self.writer.clone().unwrap());
                    let mut lck = self.channel_map.write().await;
                    lck.insert(res.id, (sender, channel.req_map.clone()));
                    drop(lck);
                    Ok(channel)
                }
            },
            _ => Err(CrosisError::UnexpectedReply(resp)),
        }
        // unimplemented!()
        // Err(())
    }

    pub async fn destroy(self) -> Result<(), CrosisError> {
        self.close().await?;
        drop(self);
        Ok(())
    }
}

async fn bg_loop(
    abort_sender: broadcast::Sender<()>,
    mut abort_signal: broadcast::Receiver<()>,
    status: Arc<RwLock<ConnectionStatus>>,
    mut msgrecv: mpsc::UnboundedReceiver<Vec<u8>>,
    chan_map: ChanMap,
    connection: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) {
    let mut lock = status.write().await;
    // TODO: Connecting -> connected
    *lock = ConnectionStatus::Connecting;
    drop(lock);

    let (mut write, mut read) = connection.split();

    let read_status = status.clone();
    let mut read_abort = abort_signal.resubscribe();
    let read_abort_sender = abort_sender.clone();
    // let read_ref = &mut read;
    let read_fut = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = read_abort.recv() => {
                    break;
                }
                _msg = read.next() => {
                    let msg = match _msg {
                        None => {
                            read_abort_sender.send(()).unwrap();
                            break
                        }
                        Some(msg) => {
                            match msg {
                                Err(err) => {
                                    println!("{}", err);
                                    read_abort_sender.send(()).unwrap();
                                    break
                                }
                                Ok(msg) => match msg {
                                    tokio_tungstenite::tungstenite::Message::Binary(msg) => msg,
                                    _ => {
                                            println!("ruh roh");
                                        read_abort_sender.send(()).unwrap();
                                        break
                                    }
                                }
                            }
                        },
                    };


                    // if let Some(goval::command::)
                    // TODO: handle error
                    let cmd = goval::Command::decode(msg.as_slice()).unwrap();

                    if let &Some(goval::command::Body::BootStatus(ref status)) = &cmd.body {
                        if status.stage() == goval::boot_status::Stage::Complete {
                            let mut lock = read_status.write().await;
                            // TODO: Connecting -> connected
                            *lock = ConnectionStatus::Connected;
                            drop(lock);
                        }
                    }
                    // println!("{cmd:#?}");

                    let map = chan_map.read().await;
                    // TODO: handle error
                    if let Some((sender, _reqs)) = map.get(&cmd.channel) {

                        if !cmd.r#ref.is_empty() {

                            let mut reqs = _reqs.write().await;

                            // println!("{}", reqs.contains_key(&cmd.r#ref));
                            if let Some(req_resp) = reqs.remove(&cmd.r#ref) {
                                // TODO: handle error
                                req_resp.send(cmd.clone()).unwrap();
                            }
                        }

                        sender.send(cmd).unwrap();
                        drop(map);
                    } else {
                        println!("Dropped msg... {cmd:#?}");
                    }
                    // println!("rx1 completed first with {:?}", val);
                }
            }
        }

        read
    });

    let ping_interval = Duration::from_millis(500);
    let _ping = goval::Command {
        channel: 0,
        body: Some(goval::command::Body::Ping(goval::Ping {})),
        ..Default::default()
    };
    let ping_msg = _ping.encode_to_vec();
    let write_fut = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = abort_signal.recv() => {
                    break;
                }
                _msg = msgrecv.recv() => {
                    let msg = match _msg {
                        None => {
                            abort_sender.send(()).unwrap();
                            break
                        }
                        Some(msg) => msg,
                    };
                    // println!("{msg:#?}");
                    if write.send(tokio_tungstenite::tungstenite::Message::Binary(msg)).await.is_err() {
                        abort_sender.send(()).unwrap();
                        break
                    }
                }
                _ = tokio::time::sleep(ping_interval) => {
                    // TODO: generate ref attr on the fly
                    if write.send(tokio_tungstenite::tungstenite::Message::Binary(ping_msg.clone())).await.is_err() {
                        abort_sender.send(()).unwrap();
                        break
                    }
                }
            }
        }

        (write, msgrecv)
    });

    let (read_res, _write_res) = tokio::join!(read_fut, write_fut);
    let mut lock = status.write().await;
    *lock = ConnectionStatus::Disconnected;
    drop(lock);
    let (write_res, mut msgrecv) = _write_res.unwrap();
    read_res
        .unwrap()
        .reunite(write_res)
        .unwrap()
        .close(None)
        .await
        .unwrap();

    // close after client status is updated so no race condition stuff
    msgrecv.close();
}

#[readonly::make]
pub struct Channel {
    pub id: i32,
    client_status: Arc<RwLock<ConnectionStatus>>,
    client_send: mpsc::UnboundedSender<Vec<u8>>,
    inner_channel: mpsc::UnboundedReceiver<goval::Command>,
    req_map: ReqMap,
}

impl Channel {
    fn new(
        id: i32,
        client_status: Arc<RwLock<ConnectionStatus>>,
        client_send: mpsc::UnboundedSender<Vec<u8>>,
    ) -> (Channel, mpsc::UnboundedSender<goval::Command>) {
        let (sender, recv) = mpsc::unbounded_channel();

        (
            Channel {
                id,
                client_status,
                client_send,
                inner_channel: recv,
                req_map: Arc::new(RwLock::new(HashMap::new())),
            },
            sender,
        )
    }

    /// Get next message sent to channel
    pub async fn next(&mut self) -> Result<goval::Command, CrosisError> {
        match self.inner_channel.recv().await {
            Some(msg) => Ok(msg),
            None => Err(CrosisError::GenericError(
                "TODO: write better error".to_string(),
            )),
        }
    }

    pub async fn send(&self, mut msg: goval::Command) {
        msg.channel = self.id;
        self.client_send.send(msg.encode_to_vec()).unwrap();
    }

    pub async fn request(&self, mut msg: goval::Command) -> Result<goval::Command, ()> {
        let mut reqs = self.req_map.write().await;

        // This is to match @replit/crosis, might benchmark and find faster random id
        let cmd_ref = generate_ref();

        msg.r#ref.clone_from(&cmd_ref);

        let (send, recv) = oneshot::channel();
        reqs.insert(cmd_ref, send);
        drop(reqs);

        self.send(msg).await;

        Ok(recv.await.unwrap())
    }
}

fn generate_ref() -> String {
    let mut result = vec![];
    let mut _x = fastrand::choose_multiple(10u32.pow(8)..(10u32.pow(9) - 1), 2);

    let mut x: u64 = (_x[0].to_string() + &_x[1].to_string()).parse().unwrap();

    loop {
        let m = x % 36;
        x /= 36;

        // will panic if you use a bad radix (< 2 or > 36).
        result.push(std::char::from_digit(m.try_into().unwrap(), 36).unwrap());
        if x == 0 {
            break;
        }
    }
    result.into_iter().rev().collect()
}

#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize))]
pub struct GovalMetadata {
    #[cfg_attr(feature = "serde", serde(rename = "conmanURL"))]
    pub conman_url: String,
    pub gurl: String,
    pub token: String,
}

#[derive(Debug)]
pub enum FetchConnectionMetadataError {
    Abort,
    Retriable,
    Other(CrosisError),
}

pub type FetchConnectionMetadataResult = Result<GovalMetadata, FetchConnectionMetadataError>;

#[async_trait]
pub trait ConnectionMetadataFetcher {
    async fn fetch(&self) -> FetchConnectionMetadataResult;
}
