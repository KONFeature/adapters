//! Steery's official `daytona` adapter. It provides `sandboxes` (PROTOCOL.md §3.2, §5.2) on
//! Daytona, through Daytona's published REST API.
//!
//! Its client is built from the settings `initialize` passes, and from nothing else: the
//! sandbox, its image, credentials and network are the company's and Daytona's, so no
//! sandbox is given an image, environment, secret or network setting of the adapter's own,
//! and Daytona's auto-stop stays the sandbox's idle timer.

mod api;

use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256};
use steery_sdk::wire::{self, sandbox};
use steery_sdk::{Error, PROTOCOL, Program, method, refusal};

use api::{BuildInfo, Create, Daytona, Deadline, Fault};

/// The label every sandbox this adapter makes carries, and every list is filtered by.
const MINE: (&str, &str) = ("steery", "sandbox");
/// The label that finds a sandbox again by the runtime's key.
const KEY_LABEL: &str = "steery-key";
/// The host Daytona's SSH access docs name, where its answer names none.
const SSH_HOST: &str = "ssh.app.daytona.io";
/// How long a channel's SSH access token lives, in minutes.
const SSH_MINUTES: u32 = 60;
/// Seconds the adapter waits for Daytona, inside the runtime's bounds (§9: 300 s, 60 s).
const LONG_SECONDS: u64 = 280;
const CHANGE_SECONDS: u64 = 55;
/// Seconds for one file call in the sandbox after an upload: a size check, a chmod, a removal.
const FILE_SECONDS: Duration = Duration::from_secs(5);
/// How often a sandbox changing state is read again; an archive takes minutes.
const POLL: Duration = Duration::from_secs(1);
const ARCHIVE_POLL: Duration = Duration::from_secs(2);
/// The states a sandbox that is being changed fails in.
const FAILED: &[&str] = &["error", "build_failed"];
/// The longest line a refusal carries.
const LINE_CHARS: usize = 300;

#[tokio::main(flavor = "current_thread")]
async fn main() -> io::Result<()> {
    steery_sdk::serve(|_notifier| Adapter::default()).await
}

/// The adapter: a Daytona client once `initialize` gave an API key, and none before.
#[derive(Default)]
struct Adapter {
    daytona: OnceLock<Option<Daytona>>,
}

impl Program for Adapter {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        if !params.protocol.contains(&PROTOCOL) {
            return Err(Error::invalid(format!(
                "this adapter speaks protocol {PROTOCOL} only"
            )));
        }
        let given = |name: &str| params.settings.get(name).filter(|value| !value.is_empty());
        let daytona = match given("DAYTONA_API_KEY") {
            Some(key) => Some(
                Daytona::new(
                    key.clone(),
                    given("DAYTONA_API_URL").map(String::as_str),
                    given("DAYTONA_TARGET").cloned(),
                )
                .map_err(|why| Error::refused(refusal::REFUSED, format!("Daytona: {why}")))?,
            ),
            None => None,
        };
        let _ = self.daytona.set(daytona);
        Ok(wire::Initialized {
            protocol: PROTOCOL,
            program: wire::ProgramInfo {
                id: "daytona".parse().map_err(Error::invalid)?,
                kind: wire::Kind::Machine,
                release: env!("CARGO_PKG_VERSION").into(),
                label: "Daytona".into(),
                short: None,
            },
            harness: None,
            machine: Some(wire::Machine::Sandboxes(wire::SandboxFacts {
                keeps: wire::Survives::Filesystem,
                persist: None,
                idle: wire::Idle::Activity,
                optional: vec![wire::Optional::Archive, wire::Optional::Keepalive],
                channel: wire::Channel::Argv,
                os: wire::Os::Linux,
            })),
            settings: vec![
                setting(
                    "DAYTONA_API_KEY",
                    true,
                    true,
                    "An API key from app.daytona.io/dashboard/keys",
                ),
                setting(
                    "DAYTONA_API_URL",
                    false,
                    false,
                    "Your Daytona API URL, for a self-hosted Daytona; absent: https://app.daytona.io/api",
                ),
                setting(
                    "DAYTONA_TARGET",
                    false,
                    false,
                    "The Daytona region sandboxes are made in (us, eu); absent: your organization's default",
                ),
            ],
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        use steery_sdk::{params as given, result};
        let daytona = || {
            self.daytona.get().and_then(Option::as_ref).ok_or_else(|| {
                Error::refused(refusal::UNTRUSTED, "Daytona: DAYTONA_API_KEY is not set")
            })
        };
        match method {
            method::SANDBOXES_LIST => result(list(daytona()?, given(params)?).await?),
            method::SANDBOX_ENSURE => result(ensure(daytona()?, given(params)?).await?),
            method::SANDBOX_START => result(start(daytona()?, given(params)?).await?),
            method::SANDBOX_STOP => result(stop(daytona()?, given(params)?).await?),
            method::SANDBOX_DELETE => result(delete(daytona()?, given(params)?).await?),
            method::SANDBOX_ARCHIVE => result(archive(daytona()?, given(params)?).await?),
            method::SANDBOX_KEEPALIVE => result(keepalive(daytona()?, given(params)?).await?),
            method::SANDBOX_PUT => result(put(daytona()?, given(params)?).await?),
            method::SANDBOX_CHANNEL => result(channel(daytona()?, given(params)?).await?),
            _ => Err(Error::Unknown),
        }
    }
}

fn setting(name: &str, secret: bool, required: bool, about: &str) -> wire::Setting {
    wire::Setting {
        name: name.into(),
        secret,
        required,
        about: about.into(),
    }
}

/// The refusal a fault is; `otherwise` where it is none of not found, auth, network or time.
/// Its line never holds the API key.
fn refuse(daytona: &Daytona, fault: Fault, otherwise: &'static str) -> Error {
    let (word, line) = match fault {
        Fault::NotFound(line) => (refusal::NOT_FOUND, line),
        Fault::Untrusted(line) => (refusal::UNTRUSTED, line),
        Fault::Offline(line) => (refusal::OFFLINE, line),
        // Daytona was reached: a call that ran out of time is not offline.
        Fault::TimedOut(line) => (refusal::REFUSED, format!("{line} (timed out)")),
        Fault::Conflict(line) | Fault::Failed(line) => (otherwise, line),
    };
    Error::refused(word, one_line(daytona.key(), &line))
}

/// `Daytona: <text>` on one line, without `key`, at most [`LINE_CHARS`] characters.
fn one_line(key: &str, text: &str) -> String {
    let text = text.replace(key, "[redacted]");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = match text.char_indices().nth(LINE_CHARS - 1) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    };
    format!("Daytona: {text}")
}

/// A sandbox's id, never empty.
fn named(id: &str) -> Result<&str, Error> {
    if id.is_empty() {
        Err(Error::invalid("sandbox must be a non-empty string"))
    } else {
        Ok(id)
    }
}

/// Milliseconds since 1970 of a time Daytona writes (RFC 3339); 0 where it is none.
fn millis(at: Option<&str>) -> u64 {
    at.and_then(|at| {
        time::OffsetDateTime::parse(at, &time::format_description::well_known::Rfc3339).ok()
    })
    .and_then(|at| u64::try_from(at.unix_timestamp_nanos() / 1_000_000).ok())
    .unwrap_or(0)
}

/// One argument as a POSIX shell reads it back.
fn quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_/.:=@%+,-".contains(&byte));
    if plain {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

async fn list(daytona: &Daytona, params: sandbox::List) -> Result<sandbox::Listed, Error> {
    let mut labels = params.labels.unwrap_or_default();
    labels.insert(MINE.0.into(), MINE.1.into());
    let found = daytona
        .list(&labels, Deadline::after(CHANGE_SECONDS))
        .await
        .map_err(|fault| refuse(daytona, fault, refusal::REFUSED))?;
    let sandboxes = found
        .into_iter()
        .map(|sandbox| sandbox::Sandbox {
            state: sandbox.state().to_owned(),
            created_at: millis(sandbox.created_at.as_deref()),
            labels: sandbox.labels.unwrap_or_default(),
            sandbox: sandbox.id,
        })
        .collect();
    Ok(sandbox::Listed { sandboxes })
}

/// The sandbox made for `key`, or one made for it now from the source asked, or from
/// Daytona's default snapshot, and started.
///
/// It is found by its name, which Daytona keeps unique and reads back at once; its listing
/// lags a new sandbox by a second or more, so finding it by its labels would make a second
/// one for an ensure sent again in that time. Two ensures for one key make one sandbox: the
/// later one's create is refused as a conflict and finds the other's.
async fn ensure(daytona: &Daytona, params: sandbox::Ensure) -> Result<sandbox::Named, Error> {
    if params.key.is_empty() {
        return Err(Error::invalid("key must be a non-empty string"));
    }
    let deadline = Deadline::after(LONG_SECONDS);
    let name = name_of(&params.key);
    let mut labels = params.labels;
    labels.insert(MINE.0.to_owned(), MINE.1.to_owned());
    labels.insert(KEY_LABEL.to_owned(), params.key.clone());
    let (snapshot, build_info) = match &params.source {
        Some(sandbox::Source::Snapshot(snapshot)) => (Some(snapshot.as_str()), None),
        Some(sandbox::Source::Image(reference)) => (None, Some(BuildInfo::image(reference))),
        None => (None, None),
    };
    let create = Create {
        name: &name,
        labels: &labels,
        snapshot,
        build_info,
        target: daytona.target(),
    };
    let ensured = async {
        loop {
            match daytona.get(&name, deadline).await {
                Ok(found) if !owned(&found, &params.key) => {
                    return Err(Fault::Failed(format!(
                        "a sandbox named {name} is not the one Steery made for this key: its labels differ"
                    )));
                }
                // A sandbox being deleted keeps its name until it is gone.
                Ok(found) if matches!(found.state(), "destroying" | "destroyed") => {}
                Ok(found) => return Ok(found.id),
                Err(Fault::NotFound(_)) => match daytona.create(&create, deadline).await {
                    Ok(made) => {
                        if made.state() != "started" {
                            daytona
                                .wait(&made.id, &["started"], FAILED, "start", POLL, deadline)
                                .await?;
                        }
                        return Ok(made.id);
                    }
                    Err(Fault::Conflict(_)) => {}
                    Err(fault) => return Err(fault),
                },
                Err(fault) => return Err(fault),
            }
            let Some(left) = deadline.left() else {
                return Err(Fault::TimedOut(format!(
                    "the sandbox named {name} was still being deleted after {LONG_SECONDS} s"
                )));
            };
            tokio::time::sleep(POLL.min(left)).await;
        }
    };
    let sandbox = ensured
        .await
        .map_err(|fault| refuse(daytona, fault, refusal::START_FAILED))?;
    Ok(sandbox::Named { sandbox })
}

/// The name of the sandbox for `key`: `steery-` and the first 128 bits of its SHA-256, in
/// hex, so any key makes a name Daytona takes.
fn name_of(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    let hex: String = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("steery-{hex}")
}

/// Whether `sandbox` carries this adapter's labels for `key`.
fn owned(sandbox: &api::Sandbox, key: &str) -> bool {
    sandbox.labels.as_ref().is_some_and(|labels| {
        labels.get(MINE.0).map(String::as_str) == Some(MINE.1)
            && labels.get(KEY_LABEL).map(String::as_str) == Some(key)
    })
}

async fn start(daytona: &Daytona, params: sandbox::Target) -> Result<sandbox::State, Error> {
    let id = named(&params.sandbox)?;
    let deadline = Deadline::after(LONG_SECONDS);
    let started = async {
        daytona.act(id, "start", deadline).await?;
        daytona
            .wait(id, &["started"], FAILED, "start", POLL, deadline)
            .await
    };
    let state = started
        .await
        .map_err(|fault| refuse(daytona, fault, refusal::START_FAILED))?;
    Ok(sandbox::State { state })
}

async fn stop(daytona: &Daytona, params: sandbox::Target) -> Result<sandbox::State, Error> {
    let id = named(&params.sandbox)?;
    let state = stopped(daytona, id, Deadline::after(CHANGE_SECONDS))
        .await
        .map_err(|fault| refuse(daytona, fault, refusal::REFUSED))?;
    Ok(sandbox::State { state })
}

/// Stops `id` and waits until it is stopped; an ephemeral sandbox may be destroyed instead.
async fn stopped(daytona: &Daytona, id: &str, deadline: Deadline) -> Result<String, Fault> {
    daytona.act(id, "stop", deadline).await?;
    daytona
        .wait(
            id,
            &["stopped", "destroyed"],
            FAILED,
            "stop",
            POLL,
            deadline,
        )
        .await
}

async fn delete(daytona: &Daytona, params: sandbox::Target) -> Result<sandbox::State, Error> {
    let id = named(&params.sandbox)?;
    let deadline = Deadline::after(CHANGE_SECONDS);
    let deleted = async {
        daytona.delete(id, deadline).await?;
        daytona
            .wait(id, &["destroyed"], FAILED, "delete", POLL, deadline)
            .await
    };
    let state = deleted
        .await
        .map_err(|fault| refuse(daytona, fault, refusal::REFUSED))?;
    Ok(sandbox::State { state })
}

/// Archive: Daytona archives only a stopped sandbox, so a started one is stopped first, and
/// moving its filesystem to object storage takes minutes.
async fn archive(daytona: &Daytona, params: sandbox::Target) -> Result<sandbox::State, Error> {
    let id = named(&params.sandbox)?;
    let deadline = Deadline::after(LONG_SECONDS);
    let archived = async {
        let was = daytona.get(id, deadline).await?;
        if was.state() == "archived" {
            return Ok("archived".to_owned());
        }
        if was.state() != "stopped" {
            stopped(daytona, id, Deadline::after(CHANGE_SECONDS / 2)).await?;
        }
        daytona.act(id, "archive", deadline).await?;
        daytona
            .wait(id, &["archived"], FAILED, "archive", ARCHIVE_POLL, deadline)
            .await
    };
    let state = archived
        .await
        .map_err(|fault| refuse(daytona, fault, refusal::REFUSED))?;
    Ok(sandbox::State { state })
}

/// Refreshes the sandbox's last activity, which Daytona's auto-stop counts from.
async fn keepalive(daytona: &Daytona, params: sandbox::Target) -> Result<wire::Empty, Error> {
    let id = named(&params.sandbox)?;
    daytona
        .act(id, "last-activity", Deadline::after(CHANGE_SECONDS))
        .await
        .map_err(|fault| refuse(daytona, fault, refusal::REFUSED))?;
    Ok(wire::Empty {})
}

/// Uploads the file, proves its size on the sandbox, and sets its mode. An upload can answer
/// before the file is whole, or time out half-written: a partial file is removed rather
/// than left to be run.
async fn put(daytona: &Daytona, params: sandbox::Put) -> Result<wire::Empty, Error> {
    let id = named(&params.sandbox)?;
    let path = params
        .path
        .to_str()
        .filter(|path| Path::new(path).is_absolute())
        .ok_or_else(|| Error::invalid("path must be an absolute UTF-8 path"))?
        .to_owned();
    if params.mode > 0o7777 {
        return Err(Error::invalid("mode must be Unix mode bits"));
    }
    let content = base64::engine::general_purpose::STANDARD
        .decode(&params.content)
        .map_err(|e| Error::invalid(format!("content must be base64: {e}")))?;
    let length = content.len() as u64;
    let deadline = Deadline::after(LONG_SECONDS);
    let fail = |fault| refuse(daytona, fault, refusal::REFUSED);
    let sandbox = daytona.get(id, deadline).await.map_err(fail)?;
    let toolbox = daytona.toolbox(&sandbox).map_err(fail)?;
    let within = deadline
        .left()
        .ok_or_else(|| fail(Fault::TimedOut(format!("no time left for {path}"))))?;
    let timed_out = match daytona.upload(&toolbox, &path, content, within).await {
        Ok(()) => false,
        Err(Fault::TimedOut(_)) => true,
        Err(fault) => return Err(fail(fault)),
    };
    let written = daytona
        .size(&toolbox, &path, FILE_SECONDS)
        .await
        .unwrap_or(0);
    if timed_out || written != length {
        let why = if timed_out {
            format!("the upload timed out after {LONG_SECONDS} s")
        } else {
            "the upload ended short".to_owned()
        };
        let removed = match daytona.remove(&toolbox, &path, FILE_SECONDS).await {
            Ok(()) => "removed it".to_owned(),
            Err(Fault::NotFound(_)) => "nothing was left".to_owned(),
            Err(fault) => format!("could not remove it: {fault:?}"),
        };
        return Err(Error::refused(
            refusal::REFUSED,
            one_line(
                daytona.key(),
                &format!(
                    "{why}: {path} had {written} of {length} bytes written; {removed} (fetch a large file from inside the sandbox)"
                ),
            ),
        ));
    }
    daytona
        .chmod(&toolbox, &path, params.mode, FILE_SECONDS)
        .await
        .map_err(fail)?;
    Ok(wire::Empty {})
}

/// An argv channel: `ssh -T` to Daytona's gateway with a 60-minute access token, running
/// `command` in the sandbox.
async fn channel(daytona: &Daytona, params: sandbox::Channel) -> Result<sandbox::Reach, Error> {
    let id = named(&params.sandbox)?;
    if params.command.is_empty() {
        return Err(Error::invalid(
            "command must be a non-empty array of strings",
        ));
    }
    let access = daytona
        .ssh_access(id, SSH_MINUTES, Deadline::after(CHANGE_SECONDS))
        .await
        .map_err(|fault| refuse(daytona, fault, refusal::REFUSED))?;
    let (host, port) = gateway(access.ssh_command.as_deref().unwrap_or(""));
    let mut argv: Vec<String> = ["ssh", "-T", "-o", "StrictHostKeyChecking=accept-new"]
        .map(String::from)
        .into();
    if let Some(port) = port {
        argv.extend(["-p".to_owned(), port]);
    }
    argv.push(format!("{}@{host}", access.token));
    argv.push(
        params
            .command
            .iter()
            .map(|arg| quote(arg))
            .collect::<Vec<_>>()
            .join(" "),
    );
    Ok(sandbox::Reach::Argv {
        argv,
        env: BTreeMap::new(),
    })
}

/// The host and port of the command Daytona means (`ssh <token>@<host>`, with any
/// `-p <port>`); the docs' host where it names none.
fn gateway(command: &str) -> (String, Option<String>) {
    let words: Vec<&str> = command.split_whitespace().collect();
    let host = words
        .iter()
        .find_map(|word| word.rsplit_once('@').map(|(_, host)| host))
        .filter(|host| !host.is_empty())
        .unwrap_or(SSH_HOST)
        .to_owned();
    let port = words
        .iter()
        .position(|word| *word == "-p")
        .and_then(|at| words.get(at + 1))
        .map(|port| (*port).to_owned());
    (host, port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_what_a_shell_would_split() {
        assert_eq!(quote("/usr/bin/steery"), "/usr/bin/steery");
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert_eq!(quote(""), "''");
    }

    #[test]
    fn reads_the_gateway_from_daytonas_command() {
        assert_eq!(
            gateway("ssh tok@ssh.app.daytona.io"),
            ("ssh.app.daytona.io".into(), None)
        );
        assert_eq!(
            gateway("ssh -p 2222 tok@gw.example"),
            ("gw.example".into(), Some("2222".into()))
        );
        assert_eq!(gateway(""), (SSH_HOST.into(), None));
    }

    #[test]
    fn reads_daytona_times_as_milliseconds() {
        assert_eq!(millis(Some("2024-10-01T12:00:00.000Z")), 1_727_784_000_000);
        assert_eq!(millis(Some("not a time")), 0);
        assert_eq!(millis(None), 0);
    }

    #[test]
    fn keeps_the_key_out_of_a_line() {
        assert_eq!(
            one_line("sk", "bad  key sk\nhere"),
            "Daytona: bad key [redacted] here"
        );
    }
}
