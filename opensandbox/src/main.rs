//! Steery's adapter for OpenSandbox: it provides sandboxes (PROTOCOL.md §3.2, §5.2, §5.3).
//!
//! The runtime starts it and speaks the program protocol on standard input and output. It
//! talks to OpenSandbox's lifecycle API and to the execd daemon inside each sandbox over
//! their published HTTP and WebSocket APIs. The sandbox, its image, credentials and network
//! are the company's and OpenSandbox's: a sandbox is given no image, environment, secret or
//! network setting of the adapter's own.

mod channel;
mod provider;

use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};
use steery_sdk::wire::sandbox::{self, Ensure, Named, Put, Source, State};
use steery_sdk::wire::{self, Empty};
use steery_sdk::{Error, PROTOCOL, Program, method, params as given, refusal, result};

use crate::channel::Channels;
use crate::provider::{Fail, Found, Provider};

/// The metadata every sandbox this adapter makes carries, and every list is filtered by.
const MINE: (&str, &str) = ("steery", "sandbox");
/// The metadata that finds a sandbox again by the runtime's key.
const KEY_LABEL: &str = "steery-key";
/// The metadata that keeps the TTL a sandbox was made with, in seconds, for keepalive.
const TTL_LABEL: &str = "steery-ttl";
/// The TTL keepalive renews by where a sandbox kept none.
const DEFAULT_TTL_SECONDS: i64 = 3600;
/// OpenSandbox requires an entrypoint with an image; this one only keeps the sandbox up.
const IDLE_ENTRYPOINT: [&str; 3] = ["tail", "-f", "/dev/null"];
/// How long a sandbox may take to make, resume or pause, inside the runtime's bounds
/// (PROTOCOL.md §9: 300 s, 60 s).
const MAKE_WAIT: Duration = Duration::from_secs(280);
const CHANGE_WAIT: Duration = Duration::from_secs(55);
const POLL: Duration = Duration::from_secs(1);
/// The states a sandbox found by key is never reused from.
const GONE: [&str; 2] = ["Stopping", "Terminated"];

#[tokio::main(flavor = "current_thread")]
async fn main() -> io::Result<()> {
    let mut shared = None;
    let served = steery_sdk::serve(|notifier| {
        let channels = Arc::new(Channels::new(notifier));
        shared = Some(channels.clone());
        OpenSandbox {
            provider: RwLock::new(Err("initialize comes first".into())),
            channels,
        }
    })
    .await;
    if let Some(channels) = shared {
        channels.shut().await;
    }
    served
}

/// The adapter: the provider its settings name, and its open channels.
struct OpenSandbox {
    provider: RwLock<Result<Arc<Provider>, String>>,
    channels: Arc<Channels>,
}

impl OpenSandbox {
    /// The provider, or the refusal its settings are.
    fn provider(&self) -> Result<Arc<Provider>, Error> {
        match &*self.provider.read().unwrap_or_else(|e| e.into_inner()) {
            Ok(provider) => Ok(provider.clone()),
            Err(why) => Err(Error::refused(
                refusal::REFUSED,
                format!("OpenSandbox: {why}"),
            )),
        }
    }
}

impl Program for OpenSandbox {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        if !params.protocol.contains(&PROTOCOL) {
            return Err(Error::invalid(format!(
                "this adapter speaks protocol {PROTOCOL} only"
            )));
        }
        *self.provider.write().unwrap_or_else(|e| e.into_inner()) =
            Provider::new(&params.settings).map(Arc::new);
        Ok(wire::Initialized {
            protocol: PROTOCOL,
            program: wire::ProgramInfo {
                id: "opensandbox".into(),
                kind: wire::Kind::Machine,
                release: env!("CARGO_PKG_VERSION").into(),
                label: "OpenSandbox".into(),
                short: None,
            },
            harness: None,
            machine: Some(wire::Machine::Sandboxes(wire::SandboxFacts {
                keeps: wire::Survives::Filesystem,
                persist: None,
                idle: wire::Idle::Ttl,
                optional: vec![wire::Optional::Pause, wire::Optional::Keepalive],
                channel: wire::Channel::Stream,
                os: wire::Os::Linux,
            })),
            settings: vec![
                wire::Setting {
                    name: "OPENSANDBOX_URL".into(),
                    secret: false,
                    required: true,
                    about:
                        "Your OpenSandbox server's URL, as its SDKs take it: http://localhost:8080"
                            .into(),
                },
                wire::Setting {
                    name: "OPENSANDBOX_API_KEY".into(),
                    secret: true,
                    required: false,
                    about: "The server's API key (its server.api_key), where it has one".into(),
                },
            ],
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        match method {
            method::SANDBOXES_LIST => result(list(&*self.provider()?, given(params)?).await?),
            method::SANDBOX_ENSURE => result(ensure(&*self.provider()?, given(params)?).await?),
            method::SANDBOX_START => result(start(&*self.provider()?, given(params)?).await?),
            method::SANDBOX_STOP | method::SANDBOX_PAUSE => {
                result(pause(&*self.provider()?, given(params)?).await?)
            }
            method::SANDBOX_DELETE => result(delete(&*self.provider()?, given(params)?).await?),
            method::SANDBOX_KEEPALIVE => {
                result(keepalive(&*self.provider()?, given(params)?).await?)
            }
            method::SANDBOX_PUT => result(put(&*self.provider()?, given(params)?).await?),
            method::SANDBOX_CHANNEL => {
                let asked = given(params)?;
                result(self.channels.channel(self.provider()?, asked).await?)
            }
            method::STREAM_WRITE => result(self.channels.write(given(params)?).await?),
            method::STREAM_CLOSE => result(self.channels.close(given(params)?).await?),
            _ => Err(Error::Unknown),
        }
    }
}

/// `name` as given, refused where it is empty.
fn named<'a>(value: &'a str, name: &str) -> Result<&'a str, Error> {
    if value.is_empty() {
        Err(Error::invalid(format!("{name} must be a non-empty string")))
    } else {
        Ok(value)
    }
}

/// An RFC 3339 time in milliseconds since 1970.
fn millis(at: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(at)
        .ok()
        .map(|at| at.timestamp_millis())
}

/// Read the sandbox until its state is one of `want`, or until `wait` has passed.
async fn until(
    provider: &Provider,
    id: &str,
    want: &[&str],
    wait: Duration,
) -> Result<Found, Fail> {
    let deadline = Instant::now() + wait;
    loop {
        let found = provider.get(id).await?;
        let state = found.status.state.as_str();
        if want.contains(&state) {
            return Ok(found);
        }
        if state == "Failed" || state == "Terminated" {
            let said = found
                .status
                .message
                .as_deref()
                .filter(|message| !message.is_empty())
                .map(|message| format!(": {message}"))
                .unwrap_or_default();
            return Err(Fail::Refused(
                refusal::START_FAILED,
                format!("sandbox {id} is {state}{said}"),
            ));
        }
        if Instant::now() >= deadline {
            return Err(Fail::Refused(
                refusal::START_FAILED,
                format!("sandbox {id} is still {state}, not {}", want.join(" or ")),
            ));
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn list(provider: &Provider, asked: sandbox::List) -> Result<sandbox::Listed, Error> {
    let mut labels = asked.labels.unwrap_or_default();
    labels.insert(MINE.0.into(), MINE.1.into());
    let found = provider
        .listed(&labels)
        .await
        .map_err(|fail| provider.refuse(fail, refusal::REFUSED))?;
    let sandboxes = found
        .into_iter()
        .map(|found| sandbox::Sandbox {
            created_at: millis(&found.created_at)
                .and_then(|at| u64::try_from(at).ok())
                .unwrap_or(0),
            sandbox: found.id,
            state: found.status.state,
            labels: found.metadata.unwrap_or_default(),
        })
        .collect();
    Ok(sandbox::Listed { sandboxes })
}

async fn ensure(provider: &Provider, asked: Ensure) -> Result<Named, Error> {
    let key = named(&asked.key, "key")?;
    let mine = BTreeMap::from([
        (MINE.0.to_owned(), MINE.1.to_owned()),
        (KEY_LABEL.to_owned(), key.to_owned()),
    ]);
    let found = provider
        .listed(&mine)
        .await
        .map_err(|fail| provider.refuse(fail, refusal::REFUSED))?
        .into_iter()
        .find(|found| !GONE.contains(&found.status.state.as_str()));
    if let Some(found) = found {
        return Ok(Named { sandbox: found.id });
    }
    let mut metadata = asked.labels;
    metadata.extend(mine);
    // `resourceLimits` is required outside pool mode; empty, the server's own defaults apply.
    let body = match &asked.source {
        Some(Source::Image(image)) => json!({
            "image": { "uri": image },
            "entrypoint": IDLE_ENTRYPOINT,
            "resourceLimits": {},
            "metadata": metadata,
        }),
        Some(Source::Snapshot(snapshot)) => json!({
            "snapshotId": snapshot,
            "resourceLimits": {},
            "metadata": metadata,
        }),
        None => {
            return Err(Error::refused(
                refusal::NO_PLACE,
                "OpenSandbox needs an image or a snapshot to make a sandbox: it has no default. Name one as the machine's source.",
            ));
        }
    };
    let made = provider
        .create(&body)
        .await
        .map_err(|fail| provider.refuse(fail, refusal::START_FAILED))?;
    // The server gave it a TTL: keep it, so keepalive renews by the same.
    let ttl = made
        .expires_at
        .as_deref()
        .and_then(millis)
        .zip(millis(&made.created_at))
        .map(|(expires, created)| (expires - created + 500) / 1000)
        .filter(|ttl| *ttl > 0);
    if let Some(ttl) = ttl {
        let patch = json!({ TTL_LABEL: ttl.to_string() });
        if let Err(fail) = provider.patch_metadata(&made.id, &patch).await {
            eprintln!("{}", provider.line(fail.said()));
        }
    }
    Ok(Named { sandbox: made.id })
}

async fn start(provider: &Provider, asked: Named) -> Result<State, Error> {
    let id = named(&asked.sandbox, "sandbox")?;
    let started = async {
        let mut found = provider.get(id).await?;
        if found.status.state == "Pausing" {
            found = until(provider, id, &["Paused"], MAKE_WAIT).await?;
        }
        match found.status.state.as_str() {
            "Paused" => {
                provider.act(id, "resume").await?;
                found = until(provider, id, &["Running"], MAKE_WAIT).await?;
            }
            "Pending" | "Resuming" => {
                found = until(provider, id, &["Running"], MAKE_WAIT).await?;
            }
            _ => {}
        }
        Ok(State {
            state: found.status.state,
        })
    };
    started
        .await
        .map_err(|fail| provider.refuse(fail, refusal::START_FAILED))
}

/// Pause: OpenSandbox's only stop that keeps the sandbox.
async fn pause(provider: &Provider, asked: Named) -> Result<State, Error> {
    let id = named(&asked.sandbox, "sandbox")?;
    let paused = async {
        let mut found = provider.get(id).await?;
        if matches!(found.status.state.as_str(), "Pending" | "Resuming") {
            found = until(provider, id, &["Running"], CHANGE_WAIT).await?;
        }
        match found.status.state.as_str() {
            "Running" => {
                provider.act(id, "pause").await?;
                found = until(provider, id, &["Paused"], CHANGE_WAIT).await?;
            }
            "Pausing" => {
                found = until(provider, id, &["Paused"], CHANGE_WAIT).await?;
            }
            _ => {}
        }
        Ok(State {
            state: found.status.state,
        })
    };
    paused
        .await
        .map_err(|fail| provider.refuse(fail, refusal::REFUSED))
}

async fn delete(provider: &Provider, asked: Named) -> Result<State, Error> {
    let id = named(&asked.sandbox, "sandbox")?;
    let deleted = async {
        provider.delete(id).await?;
        match provider.get(id).await {
            Ok(found) => Ok(State {
                state: found.status.state,
            }),
            Err(fail) if fail.is_not_found() => Ok(State {
                state: "Terminated".into(),
            }),
            Err(fail) => Err(fail),
        }
    };
    deleted
        .await
        .map_err(|fail| provider.refuse(fail, refusal::REFUSED))
}

/// Renew the sandbox's expiration by the TTL it was made with; one with none never expires.
async fn keepalive(provider: &Provider, asked: Named) -> Result<Empty, Error> {
    let id = named(&asked.sandbox, "sandbox")?;
    let kept = async {
        let found = provider.get(id).await?;
        if found.expires_at.is_none() {
            return Ok(Empty {});
        }
        let ttl = found
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get(TTL_LABEL))
            .and_then(|ttl| ttl.parse::<i64>().ok())
            .filter(|ttl| *ttl > 0)
            .unwrap_or(DEFAULT_TTL_SECONDS);
        let expires_at = (Utc::now() + chrono::Duration::seconds(ttl))
            .to_rfc3339_opts(SecondsFormat::Secs, true);
        provider.renew(id, &expires_at).await?;
        Ok(Empty {})
    };
    kept.await
        .map_err(|fail| provider.refuse(fail, refusal::REFUSED))
}

/// Write the file through execd, check that every byte is there, and remove what is not
/// whole before refusing (PROTOCOL.md §5.2).
async fn put(provider: &Provider, asked: Put) -> Result<Empty, Error> {
    let id = named(&asked.sandbox, "sandbox")?;
    let path = asked
        .path
        .to_str()
        .filter(|path| path.starts_with('/'))
        .ok_or_else(|| Error::invalid("path must be absolute"))?;
    let content = STANDARD
        .decode(asked.content.as_bytes())
        .map_err(|_| Error::invalid("content must be base64"))?;
    let length = content.len() as u64;
    let execd = provider
        .execd(id)
        .await
        .map_err(|fail| provider.refuse(fail, refusal::REFUSED))?;
    let written = async {
        provider.upload(&execd, path, content, asked.mode).await?;
        let size = provider.size(&execd, path).await?;
        if size == length {
            Ok(())
        } else {
            Err(Fail::Other(format!(
                "upload file: {path} holds {size} of {length} bytes"
            )))
        }
    };
    match written.await {
        Ok(()) => Ok(Empty {}),
        Err(fail) => {
            if let Err(left) = provider.remove_file(&execd, path).await
                && !left.is_not_found()
            {
                eprintln!("{}", provider.line(left.said()));
            }
            Err(provider.refuse(fail, refusal::REFUSED))
        }
    }
}
