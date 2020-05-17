use deluge_macro::*;

use serde_json::Value;
use serde::{Serialize, Deserialize, Serializer};

use crate::encoding;
use crate::rpc;
use crate::receiver::MessageReceiver;
use crate::error::{Error, Result};
use crate::wtf;

use tokio_rustls::{TlsConnector, webpki, client::TlsStream};
use tokio::net::TcpStream;

use std::sync::Arc;
use std::iter::FromIterator;

use tokio::prelude::*;
use tokio::sync::{oneshot, mpsc};

type List = Vec<Value>;
// I don't expect to come across any non-string keys.
type Dict = std::collections::HashMap<String, Value>;

type WriteStream = io::WriteHalf<TlsStream<TcpStream>>;
type RequestTuple = (i64, &'static str, List, Dict);
type RpcSender = oneshot::Sender<rpc::Result<List>>;

#[derive(Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct InfoHash([u8; 20]);

impl InfoHash {
    pub fn from_hex(hex_str: &str) -> Option<Self> {
        if hex_str.len() != 40 {
            println!("len = {}", hex_str.len());
            return None;
        }

        let bytes_vec = match hex::decode(hex_str) {
            Ok(x) => x,
            Err(e) => {
                println!("{:?}", e);
                return None;
            },
        };

        let mut final_array = [0; 20];
        final_array.copy_from_slice(&bytes_vec);

        Some(Self(final_array))
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl Into<String> for InfoHash {
    fn into(self) -> String { self.to_string() }
}

impl std::convert::TryFrom<String> for InfoHash {
    type Error = &'static str;
    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        Self::from_hex(&value).ok_or("invalid infohash")
    }
}

impl std::fmt::Display for InfoHash {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl std::fmt::Debug for InfoHash {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

pub struct Session {
    stream: WriteStream,
    prev_req_id: i64,
    listeners: mpsc::Sender<(i64, RpcSender)>,
    auth_level: i64,
}

#[allow(dead_code)]
#[derive(Copy, Clone)]
pub enum FilePriority { Skip = 0, Low = 1, Normal = 4, High = 7 }
impl Default for FilePriority { fn default() -> Self { Self::Normal } }
impl Serialize for FilePriority {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_u8(*self as u8)
    }
}

#[option_struct]
#[derive(Clone, Default)]
pub struct TorrentOptions {
    pub add_paused: bool,
    pub auto_managed: bool,
    pub download_location: String,
    pub file_priorities: Vec<FilePriority>,
    pub mapped_files: std::collections::HashMap<String, String>,
    pub max_connections: i64,
    pub max_download_speed: f64,
    pub max_upload_slots: i64,
    pub max_upload_speed: f64,
    pub move_completed: bool,
    pub move_completed_path: String,
    pub name: String,
    pub owner: String,
    pub pre_allocate_storage: bool,
    pub prioritize_first_last_pieces: bool,
    pub remove_at_ratio: bool,
    pub seed_mode: bool, // Only used when adding a torrent
    pub sequential_download: bool,
    pub shared: bool,
    pub stop_at_ratio: bool,
    pub stop_ratio: f64,
    pub super_seeding: bool,
}

#[macro_export]
macro_rules! dict {
    ($($key:expr => $val:expr),*$(,)?) => {
        {
            use maplit::hashmap;
            maplit::convert_args!(
                keys=String::from,
                values=serde_json::Value::from,
                hashmap!($($key => $val),*)
            )
        }
    }
}

macro_rules! build_request {
    (
        $method:expr
        $(, [$($arg:expr),*])?
        $(, {$($kw:expr => $kwarg:expr),*})?
        $(,)?
    ) => {
        $crate::rpc::Request {
            method: $method,
            args: vec![$($(serde_json::json!($arg)),*)?],
            kwargs: dict!{$($($kw => $kwarg),*)?}
        }
    };
}

macro_rules! make_request {
    ($self:ident, $($arg:tt),*$(,)?) => {
        $self.request(build_request!($($arg),*)).await?
    }
}

macro_rules! expect {
    ($val:expr, ?$pat:pat, $expected:expr, $result:expr) => {
        match $val {
            $pat => Ok(Some($result)),
            Value::Null => Ok(None),
            x => Err(Error::expected($expected, x)),
        }
    };

    ($val:expr, $pat:pat, $expected:expr, $result:expr) => {
        match $val {
            $pat => Ok($result),
            x => Err(Error::expected($expected, x)),
        }
    }
}

macro_rules! expect_nothing {
    ($val:expr) => {
        if $val.is_empty() {
            Ok(())
        } else {
            Err(Error::expected("nothing", $val))
        }
    }
}

macro_rules! expect_val {
    ($val:expr, $pat:pat, $expected:expr, $result:expr) => {
        match $val.len() {
            1 => expect!($val.into_iter().next().unwrap(), $pat, $expected, $result),
            _ => Err(Error::expected(std::concat!("a list containing only ", $expected), $val)),
        }
    }
}

macro_rules! expect_option {
    ($val:expr, $pat:pat, $expected:expr, $result:expr) => {
        match $val.len() {
            1 => expect!($val.into_iter().next().unwrap(), ?$pat, $expected, $result),
            _ => Err(Error::expected(std::concat!("a list containing only ", $expected), $val)),
        }
    };
}

macro_rules! expect_seq {
    ($val:expr, $pat:pat, $expected_val:literal, $result:expr) => {
        $val.into_iter()
            .map(|x| match x {
                $pat => Ok($result.into()),
                v => {
                    let expected = std::concat!("a list where every item is ", $expected_val);
                    let actual = format!("a list containing {:?}", v);
                    Err(Error::expected(expected, actual))
                }
            })
            .collect()
    }
}

pub trait Query: for<'de> Deserialize<'de> {
    fn keys() -> &'static [&'static str];
}

#[allow(dead_code)]
impl Session {
    fn prepare_request(&mut self, request: rpc::Request) -> RequestTuple {
        self.prev_req_id += 1;
        (self.prev_req_id, request.method, request.args, request.kwargs)
    }

    async fn send(&mut self, req: RequestTuple) -> Result<()> {
        let body = encoding::encode(&[req]).unwrap();
        self.stream.write_u8(1).await?;
        self.stream.write_u32(body.len() as u32).await?;
        self.stream.write_all(&body).await?;
        self.stream.flush().await?;
        Ok(())
    }

    async fn request(&mut self, req: rpc::Request) -> Result<List> {
        let request = self.prepare_request(req);
        let id = request.0;

        let (sender, receiver) = oneshot::channel();
        self.listeners.send((id, sender))
            .await
            .map_err(|_| Error::ChannelClosed("rpc listeners"))?;

        self.send(request).await?;

        // This is an RPC result inside a oneshot result.
        match receiver.await {
            Ok(Ok(r)) => Ok(r), // Success
            Ok(Err(e)) => Err(Error::Rpc(e)), // RPC error
            Err(_) => Err(Error::ChannelClosed("rpc response")), // Channel error
        }
    }

    pub async fn new(endpoint: impl tokio::net::ToSocketAddrs) -> Result<Self> {
        let mut tls_config = rustls::ClientConfig::new();
        //let server_pem_file = File::open("/home/the0x539/misc_software/dtui/experiment/certs/server.pem").unwrap();
        //tls_config.root_store.add_pem_file(&mut BufReader::new(pem_file)).unwrap();
        //tls_config.root_store.add_server_trust_anchors(&webpki_roots::TLS_SERVER_ROOTS);
        tls_config.dangerous().set_certificate_verifier(Arc::new(wtf::NoCertificateVerification));
        let tls_connector = TlsConnector::from(Arc::new(tls_config));

        let tcp_stream = TcpStream::connect(endpoint).await?;
        let stupid_dns_ref = webpki::DNSNameRef::try_from_ascii_str("foo").unwrap();
        let stream = tls_connector.connect(stupid_dns_ref, tcp_stream).await?;

        let (reader, writer) = io::split(stream);
        let (request_send, request_recv) = mpsc::channel(100);

        MessageReceiver::spawn(reader, request_recv);

        Ok(Self { stream: writer, prev_req_id: 0, listeners: request_send, auth_level: 0 })
    }

    #[rpc_method(class="daemon", method="info", auth_level=0)]
    pub async fn daemon_info(&mut self) -> String;

    #[rpc_method(class="daemon", auth_level=0, client_version="2.0.4.dev23")]
    pub async fn login(&mut self, username: &str, password: &str) -> i64 {
        self.auth_level = val?;
        Ok(self.auth_level)
    }

    // TODO: make private and add register_event_handler function that takes a channel or closure
    // (haven't decided which) and possibly an enum
    #[rpc_method(class="daemon", auth_level=5)]
    pub async fn set_event_interest(&mut self, events: &[&str]) -> bool;

    #[rpc_method(class="daemon", auth_level=5)]
    pub async fn shutdown(mut self) -> () {
        // TODO: restructure the macros so that val isn't a Result
        val?;
        self.close().await
    }

    #[rpc_method(class="daemon", auth_level=5)]
    pub async fn get_method_list(&mut self) -> [String];

    #[rpc_method(class="core", auth_level=5)]
    pub async fn get_session_state(&mut self) -> [InfoHash];

    #[rpc_method(class="core", auth_level=5)]
    pub async fn get_torrent_status<T: Query>(&mut self, torrent_id: &InfoHash) -> T;

    #[rpc_method(class="core", auth_level=5)]
    pub async fn get_torrents_status<T: Query>(&mut self, filter_dict: Option<Dict>) -> Map<InfoHash, T>;

    #[rpc_method(class="core", auth_level=5)]
    pub async fn add_torrent_file(&mut self, filename: &str, filedump: &str, options: &TorrentOptions) -> Option<InfoHash>;

    #[rpc_method(class="core", auth_level=5)]
    pub async fn add_torrent_files(&mut self, torrent_files: &[(&str, &str, &TorrentOptions)]);

    // TODO: accept a guaranteed-valid struct for the magnet URI
    #[rpc_method(class="core", auth_level=5)]
    pub async fn add_torrent_magnet(&mut self, uri: &str, options: &TorrentOptions) -> InfoHash;

    // TODO: accept guaranteed-valid structs for the URL and HTTP headers
    #[rpc_method(class="core", auth_level=5)]
    pub async fn add_torrent_url(&mut self, url: &str, options: &TorrentOptions, headers: Option<Dict>) -> Option<InfoHash>;

    #[rpc_method(class="core", auth_level=5)]
    pub async fn get_config(&mut self) -> Dict;

    pub async fn close(mut self) -> Result<()> {
        self.stream.shutdown().await?;
        Ok(())
    }
}
