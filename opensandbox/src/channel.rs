//! The `stream` channel (PROTOCOL.md §5.3): a pipe-mode PTY session in execd running the
//! command, joined over its WebSocket (`components/execd/pkg/web/model/pty_ws.go`).
//! Standard output goes to the runtime as `stream/data`; standard error goes to this
//! adapter's log; `stream/closed` carries the exit code.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt as _, StreamExt as _};
use serde::Deserialize;
use serde_json::json;
use steery_sdk::wire::Empty;
use steery_sdk::wire::hosts::{
    Channel, Reach, StreamClose, StreamClosed, StreamData, StreamWrite,
};
use steery_sdk::{Error, Notifier, method, refusal};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio::task::AbortHandle;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::provider::{CHANGE, Execd, Fail, Provider, chain};

/// How long `stream/close` gives the command at each step before the next.
const CLOSE_GRACE: Duration = Duration::from_secs(2);

// execd's binary frames: the first byte says which stream the rest is.
const BIN_STDIN: u8 = 0x00;
const BIN_STDOUT: u8 = 0x01;
const BIN_STDERR: u8 = 0x02;
/// Output written before this socket joined, after an 8-byte offset.
const BIN_REPLAY: u8 = 0x03;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// execd's JSON frames, as far as the adapter reads them.
#[derive(Deserialize)]
struct ServerFrame {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    exit_code: Option<i32>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    code: Option<String>,
}

impl ServerFrame {
    fn said(&self) -> String {
        let code = self.code.as_deref().unwrap_or("error");
        match self.error.as_deref().filter(|error| !error.is_empty()) {
            Some(error) => format!("execd: {code}: {error}"),
            None => format!("execd: {code}"),
        }
    }
}

/// One open channel.
struct Open {
    stream: String,
    provider: Arc<Provider>,
    execd: Execd,
    sink: tokio::sync::Mutex<SplitSink<Socket, Message>>,
    exit: Mutex<Option<i32>>,
    /// Set once `stream/closed` is sent.
    finished: AtomicBool,
    ended: watch::Sender<bool>,
    reader: Mutex<Option<AbortHandle>>,
}

impl Open {
    fn is_ended(&self) -> bool {
        *self.ended.borrow()
    }

    /// Wait until the channel ends, or `grace` has passed.
    async fn grace(&self, grace: Duration) {
        let mut ended = self.ended.subscribe();
        let _ = timeout(grace, ended.wait_for(|ended| *ended)).await;
    }

    /// Send `message` on the socket, within the runtime's bound.
    async fn send(&self, message: Message) -> Result<(), String> {
        let mut sink = self.sink.lock().await;
        match timeout(CHANGE, sink.send(message)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(chain(&e)),
            Err(_) => Err("the socket took nothing in time".into()),
        }
    }
}

/// Every open channel, by its stream id (execd's session id).
pub struct Channels {
    notifier: Notifier,
    open: Mutex<HashMap<String, Arc<Open>>>,
}

impl Channels {
    pub fn new(notifier: Notifier) -> Self {
        Self {
            notifier,
            open: Mutex::new(HashMap::new()),
        }
    }

    fn opened(&self, stream: &str) -> Result<Arc<Open>, Error> {
        self.open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(stream)
            .cloned()
            .ok_or_else(|| {
                Error::refused(
                    refusal::NOT_FOUND,
                    format!("OpenSandbox: no open channel {stream}"),
                )
            })
    }

    /// `hosts/channel`: open a PTY session running `command` and join it.
    pub async fn channel(
        self: &Arc<Self>,
        provider: Arc<Provider>,
        asked: Channel,
    ) -> Result<Reach, Error> {
        if asked.host.is_empty() {
            return Err(Error::invalid("host must be a non-empty string"));
        }
        if asked.command.is_empty() {
            return Err(Error::invalid(
                "command must be a non-empty list of strings",
            ));
        }
        let command = asked
            .command
            .iter()
            .map(|arg| quote(arg))
            .collect::<Vec<_>>()
            .join(" ");
        let execd = provider
            .execd(&asked.host)
            .await
            .map_err(|fail| provider.refuse(fail, refusal::REFUSED))?;
        let session = provider
            .open_pty(&execd, &command)
            .await
            .map_err(|fail| provider.refuse(fail, refusal::REFUSED))?;
        let (socket, early) = match join(&execd, &session).await {
            Ok(joined) => joined,
            Err(fail) => {
                let _ = provider.delete_pty(&execd, &session).await;
                return Err(provider.refuse(fail, refusal::REFUSED));
            }
        };
        let (sink, source) = socket.split();
        let open = Arc::new(Open {
            stream: session.clone(),
            provider,
            execd,
            sink: tokio::sync::Mutex::new(sink),
            exit: Mutex::new(None),
            finished: AtomicBool::new(false),
            ended: watch::channel(false).0,
            reader: Mutex::new(None),
        });
        self.open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session.clone(), open.clone());
        // The runtime runs on one thread, and the SDK writes this call's answer before this
        // task yields again: the reader, spawned here, sends nothing before the runtime
        // has the stream's id.
        let reader = tokio::spawn(self.clone().read(open.clone(), source, early));
        *open.reader.lock().unwrap_or_else(|e| e.into_inner()) = Some(reader.abort_handle());
        Ok(Reach::Stream { stream: session })
    }

    /// Read the socket until it closes: output to the runtime, errors to the log.
    async fn read(
        self: Arc<Self>,
        open: Arc<Open>,
        mut source: SplitStream<Socket>,
        early: Vec<Message>,
    ) {
        for message in early {
            self.take(&open, message);
        }
        while let Some(message) = source.next().await {
            match message {
                Ok(Message::Close(_)) => break,
                Ok(message) => self.take(&open, message),
                Err(e) => {
                    eprintln!("[{}] execd socket: {}", open.stream, chain(&e));
                    break;
                }
            }
        }
        self.finish(&open);
    }

    /// One message from execd.
    fn take(&self, open: &Open, message: Message) {
        match message {
            Message::Text(text) => {
                let Ok(frame) = serde_json::from_str::<ServerFrame>(&text) else {
                    return;
                };
                match frame.kind.as_str() {
                    "exit" => {
                        // execd says -1 for a command a signal ended: it has no exit code.
                        *open.exit.lock().unwrap_or_else(|e| e.into_inner()) =
                            frame.exit_code.filter(|code| *code >= 0);
                    }
                    "error" => eprintln!("[{}] {}", open.stream, frame.said()),
                    _ => {}
                }
            }
            Message::Binary(bytes) => {
                let Some((&kind, rest)) = bytes.split_first() else {
                    return;
                };
                let payload = match kind {
                    BIN_REPLAY => rest.get(8..).unwrap_or_default(),
                    _ => rest,
                };
                if payload.is_empty() {
                    return;
                }
                match kind {
                    BIN_STDOUT | BIN_REPLAY => self.notifier.notify(
                        method::STREAM_DATA,
                        json(StreamData {
                            stream: open.stream.clone(),
                            bytes: STANDARD.encode(payload),
                        }),
                    ),
                    BIN_STDERR => eprintln!(
                        "[{}] {}",
                        open.stream,
                        String::from_utf8_lossy(payload).trim_end()
                    ),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// The channel ended: forget it and say so, once.
    fn finish(&self, open: &Open) {
        if open.finished.swap(true, Ordering::SeqCst) {
            return;
        }
        {
            let mut channels = self.open.lock().unwrap_or_else(|e| e.into_inner());
            if channels
                .get(&open.stream)
                .is_some_and(|known| std::ptr::eq(Arc::as_ptr(known), open))
            {
                channels.remove(&open.stream);
            }
        }
        let exit = *open.exit.lock().unwrap_or_else(|e| e.into_inner());
        self.notifier.notify(
            method::STREAM_CLOSED,
            json(StreamClosed {
                stream: open.stream.clone(),
                exit,
            }),
        );
        open.ended.send_replace(true);
    }

    /// `stream/write`: hand the bytes to the command's standard input.
    pub async fn write(&self, asked: StreamWrite) -> Result<Empty, Error> {
        let open = self.opened(&asked.stream)?;
        let bytes = STANDARD
            .decode(asked.bytes.as_bytes())
            .map_err(|_| Error::invalid("bytes must be base64"))?;
        if open.is_ended() {
            return Err(closing());
        }
        let mut frame = Vec::with_capacity(1 + bytes.len());
        frame.push(BIN_STDIN);
        frame.extend_from_slice(&bytes);
        open.send(Message::Binary(frame.into()))
            .await
            .map_err(|_| closing())?;
        Ok(Empty {})
    }

    /// `stream/close`: end the channel. execd's frames have no way to close only standard
    /// input, so the command is sent SIGHUP, as a terminal's closing would; whatever still
    /// runs after a grace is ended with the session, and then the socket is closed.
    pub async fn close(&self, asked: StreamClose) -> Result<Empty, Error> {
        let open = self.opened(&asked.stream)?;
        if !open.is_ended() {
            let signal = json!({ "type": "signal", "signal": "SIGHUP" }).to_string();
            if open.send(Message::text(signal)).await.is_ok() {
                open.grace(CLOSE_GRACE).await;
            }
        }
        if let Err(fail) = open.provider.delete_pty(&open.execd, &open.stream).await
            && !fail.is_not_found()
        {
            let line = open.provider.line(fail.said());
            eprintln!("[{}] {line}", open.stream);
        }
        open.grace(CLOSE_GRACE).await;
        if !open.is_ended() {
            let frame = CloseFrame {
                code: CloseCode::Normal,
                reason: "closed by steery".into(),
            };
            let _ = open.send(Message::Close(Some(frame))).await;
            open.grace(CLOSE_GRACE).await;
        }
        if !open.is_ended() {
            // execd never answered the close: the socket is dropped with its reader.
            if let Some(reader) = open.reader.lock().unwrap_or_else(|e| e.into_inner()).take() {
                reader.abort();
            }
            self.finish(&open);
        }
        Ok(Empty {})
    }

    /// The runtime said shutdown: close every socket still open.
    pub async fn shut(&self) {
        let open: Vec<Arc<Open>> = self
            .open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        for open in open {
            let frame = CloseFrame {
                code: CloseCode::Away,
                reason: "steery adapter shut down".into(),
            };
            let mut sink = open.sink.lock().await;
            let _ = timeout(
                Duration::from_secs(1),
                sink.send(Message::Close(Some(frame))),
            )
            .await;
        }
    }
}

/// Join the session's WebSocket and wait for execd's `connected`; binary frames that came
/// first are kept for the reader.
async fn join(execd: &Execd, session: &str) -> Result<(Socket, Vec<Message>), Fail> {
    let what = "join pty session";
    let mut url = execd.url(&["pty", session, "ws"]);
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme)
        .map_err(|()| Fail::Other(format!("{what}: no WebSocket URL for {url}")))?;
    url.query_pairs_mut().append_pair("pty", "0");
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|e| Fail::Other(format!("{what}: {}", chain(&e))))?;
    for (name, value) in &execd.headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| Fail::Other(format!("{what}: the endpoint names a bad header")))?;
        let value = HeaderValue::from_str(value)
            .map_err(|_| Fail::Other(format!("{what}: the endpoint names a bad header")))?;
        request.headers_mut().insert(name, value);
    }
    let joined = timeout(CHANGE, async {
        let (mut socket, _) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| socket_fail(what, &e))?;
        let mut early = Vec::new();
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    let Ok(frame) = serde_json::from_str::<ServerFrame>(&text) else {
                        continue;
                    };
                    match frame.kind.as_str() {
                        "connected" => return Ok((socket, early)),
                        "error" => return Err(Fail::Other(format!("{what}: {}", frame.said()))),
                        _ => {}
                    }
                }
                Some(Ok(Message::Close(frame))) => {
                    let why = frame
                        .map(|frame| format!(" ({} {})", u16::from(frame.code), frame.reason))
                        .unwrap_or_default();
                    return Err(Fail::Other(format!(
                        "{what}: execd closed the channel before it was joined{why}"
                    )));
                }
                Some(Ok(message @ Message::Binary(_))) => early.push(message),
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(socket_fail(what, &e)),
                None => {
                    return Err(Fail::Other(format!(
                        "{what}: execd closed the channel before it was joined"
                    )));
                }
            }
        }
    })
    .await;
    joined.unwrap_or_else(|_| Err(Fail::Offline(format!("{what}: execd said nothing in time"))))
}

fn socket_fail(what: &str, error: &tungstenite::Error) -> Fail {
    match error {
        tungstenite::Error::Http(response) => Fail::Http {
            status: response.status().as_u16(),
            line: format!("{what}: HTTP {}", response.status()),
        },
        tungstenite::Error::Io(_) | tungstenite::Error::ConnectionClosed => {
            Fail::Offline(format!("{what}: {}", chain(error)))
        }
        _ => Fail::Other(format!("{what}: {}", chain(error))),
    }
}

fn closing() -> Error {
    Error::refused(refusal::OFFLINE, "OpenSandbox: the channel is closing")
}

fn json<T: serde::Serialize>(value: T) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or_default()
}

/// One argument as a POSIX shell reads it back.
fn quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_/.:=@%+,-".contains(&b));
    if plain {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::quote;

    #[test]
    fn arguments_read_back_as_given() {
        assert_eq!(quote("steery"), "steery");
        assert_eq!(quote("/usr/bin/x=1"), "/usr/bin/x=1");
        assert_eq!(quote(""), "''");
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert_eq!(quote("$HOME"), "'$HOME'");
    }
}
