//! Daytona's published REST API, as far as this adapter calls it: the control plane
//! (`openapi.json`: sandboxes, their lifecycle, SSH access) and each sandbox's toolbox
//! (`toolbox-openapi.json`: its files), reached through the toolbox proxy Daytona names.

use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::{Method, RequestBuilder, Response, StatusCode, multipart};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::time::Instant;

/// Daytona's own API, where the person names none.
pub const DEFAULT_API: &str = "https://app.daytona.io/api";

/// Most sandboxes one page of `GET /sandbox` holds.
const PAGE: u32 = 200;

/// How a call to Daytona failed.
#[derive(Debug)]
pub enum Fault {
    /// Daytona has no such sandbox or file (HTTP 404).
    NotFound(String),
    /// Daytona refused the key (HTTP 401, 403).
    Untrusted(String),
    /// Daytona could not be reached.
    Offline(String),
    /// Daytona was reached and did not finish in time.
    TimedOut(String),
    /// Daytona already has what was to be made: a sandbox by that name (HTTP 409).
    Conflict(String),
    /// Anything else Daytona answered, or a sandbox that ended in a state it was not asked for.
    Failed(String),
}

/// A sandbox as Daytona answers it, as far as the adapter reads it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sandbox {
    pub id: String,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub labels: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub error_reason: Option<String>,
    #[serde(default)]
    pub toolbox_proxy_url: Option<String>,
}

impl Sandbox {
    /// Its state, `unknown` where Daytona names none.
    pub fn state(&self) -> &str {
        self.state.as_deref().unwrap_or("unknown")
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Page {
    items: Vec<Sandbox>,
    #[serde(default)]
    next_cursor: Option<String>,
}

/// What a sandbox is made from, as `POST /sandbox` takes it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Create<'a> {
    pub name: &'a str,
    pub labels: &'a BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_info: Option<BuildInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<&'a str>,
}

/// An image to build a sandbox from: Daytona builds it from a Dockerfile.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildInfo {
    pub dockerfile_content: String,
}

impl BuildInfo {
    /// The image `reference` as it is.
    pub fn image(reference: &str) -> Self {
        Self {
            dockerfile_content: format!("FROM {reference}\n"),
        }
    }
}

/// SSH access to a sandbox, as `POST /sandbox/{id}/ssh-access` answers it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshAccess {
    pub token: String,
    #[serde(default)]
    pub ssh_command: Option<String>,
}

#[derive(Deserialize)]
struct FileInfo {
    size: u64,
}

/// A time by which a method answers.
#[derive(Clone, Copy)]
pub struct Deadline {
    at: Instant,
    seconds: u64,
}

impl Deadline {
    pub fn after(seconds: u64) -> Self {
        Self {
            at: Instant::now() + Duration::from_secs(seconds),
            seconds,
        }
    }

    pub fn seconds(&self) -> u64 {
        self.seconds
    }

    /// The time left, or none.
    pub fn left(&self) -> Option<Duration> {
        self.at
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
    }

    fn within(&self) -> Result<Duration, Fault> {
        self.left()
            .ok_or_else(|| Fault::TimedOut(format!("no answer within {} s", self.seconds)))
    }
}

/// A client of one Daytona organization, by its API key.
pub struct Daytona {
    http: reqwest::Client,
    api: String,
    key: String,
    target: Option<String>,
}

impl Daytona {
    pub fn new(key: String, api: Option<&str>, target: Option<String>) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .user_agent(concat!(
                "steery-adapter-daytona/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .map_err(|e| e.to_string())?;
        let api = api.unwrap_or(DEFAULT_API).trim_end_matches('/').to_owned();
        Ok(Self {
            http,
            api,
            key,
            target,
        })
    }

    /// The API key, to keep out of every line.
    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn target(&self) -> Option<&str> {
        self.target.as_deref()
    }

    fn request(&self, method: Method, url: &str, within: Duration) -> RequestBuilder {
        self.http
            .request(method, url)
            .bearer_auth(&self.key)
            .timeout(within)
    }

    fn control(
        &self,
        method: Method,
        path: &str,
        deadline: Deadline,
    ) -> Result<RequestBuilder, Fault> {
        Ok(self.request(method, &format!("{}{path}", self.api), deadline.within()?))
    }

    /// Every sandbox carrying all of `labels`, page by page. Daytona's listing is an index
    /// that may lag a change by a second or more.
    pub async fn list(
        &self,
        labels: &BTreeMap<String, String>,
        deadline: Deadline,
    ) -> Result<Vec<Sandbox>, Fault> {
        let labels = serde_json::to_string(labels).map_err(|e| Fault::Failed(e.to_string()))?;
        let limit = PAGE.to_string();
        let mut found = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut query = vec![("labels", labels.clone()), ("limit", limit.clone())];
            if let Some(cursor) = cursor.take() {
                query.push(("cursor", cursor));
            }
            let page: Page = json(
                send(
                    self.control(Method::GET, "/sandbox", deadline)?
                        .query(&query),
                )
                .await?,
            )
            .await?;
            found.extend(page.items);
            match page.next_cursor.filter(|next| !next.is_empty()) {
                Some(next) => cursor = Some(next),
                None => return Ok(found),
            }
        }
    }

    pub async fn get(&self, id: &str, deadline: Deadline) -> Result<Sandbox, Fault> {
        json(send(self.control(Method::GET, &sandbox_path(id, ""), deadline)?).await?).await
    }

    pub async fn create(&self, create: &Create<'_>, deadline: Deadline) -> Result<Sandbox, Fault> {
        json(
            send(
                self.control(Method::POST, "/sandbox", deadline)?
                    .json(create),
            )
            .await?,
        )
        .await
    }

    /// `POST /sandbox/{id}/{action}`: `start`, `stop`, `archive`, `last-activity`.
    pub async fn act(&self, id: &str, action: &str, deadline: Deadline) -> Result<(), Fault> {
        send(self.control(Method::POST, &sandbox_path(id, action), deadline)?)
            .await
            .map(drop)
    }

    pub async fn delete(&self, id: &str, deadline: Deadline) -> Result<(), Fault> {
        send(self.control(Method::DELETE, &sandbox_path(id, ""), deadline)?)
            .await
            .map(drop)
    }

    pub async fn ssh_access(
        &self,
        id: &str,
        minutes: u32,
        deadline: Deadline,
    ) -> Result<SshAccess, Fault> {
        let request = self
            .control(Method::POST, &sandbox_path(id, "ssh-access"), deadline)?
            .query(&[("expiresInMinutes", minutes)]);
        json(send(request).await?).await
    }

    /// Polls `id` until it is in one of `targets`, every `every`. A state in `fails`, or
    /// `destroyed` where it is not a target, fails; a sandbox Daytona no longer has is
    /// `destroyed`. A failed reading is read again until the deadline, as Daytona's own
    /// clients do, but a refused key or a missing sandbox is final.
    pub async fn wait(
        &self,
        id: &str,
        targets: &[&str],
        fails: &[&str],
        what: &str,
        every: Duration,
        deadline: Deadline,
    ) -> Result<String, Fault> {
        loop {
            let reading = match self.get(id, deadline).await {
                Ok(sandbox) => Ok(sandbox),
                Err(Fault::NotFound(_)) if targets.contains(&"destroyed") => {
                    return Ok("destroyed".into());
                }
                Err(fault @ (Fault::NotFound(_) | Fault::Untrusted(_))) => return Err(fault),
                Err(fault) => Err(fault),
            };
            let still = match reading {
                Ok(sandbox) => {
                    let state = sandbox.state();
                    if targets.contains(&state) {
                        return Ok(state.to_owned());
                    }
                    if fails.contains(&state) || state == "destroyed" {
                        let reason = sandbox.error_reason.as_deref().unwrap_or("no reason given");
                        return Err(Fault::Failed(format!(
                            "sandbox {id} failed to {what}: {state}: {reason}"
                        )));
                    }
                    state.to_owned()
                }
                Err(fault) if deadline.left().is_some() => {
                    eprintln!("daytona: reading sandbox {id} again: {fault:?}");
                    "unread".to_owned()
                }
                Err(fault) => return Err(fault),
            };
            let Some(left) = deadline.left() else {
                return Err(Fault::TimedOut(format!(
                    "sandbox {id} still {still} after {} s, not {what}",
                    deadline.seconds()
                )));
            };
            tokio::time::sleep(every.min(left)).await;
        }
    }

    /// The base of `sandbox`'s toolbox API.
    pub fn toolbox(&self, sandbox: &Sandbox) -> Result<String, Fault> {
        let proxy = sandbox
            .toolbox_proxy_url
            .as_deref()
            .filter(|url| !url.is_empty())
            .ok_or_else(|| Fault::Failed(format!("sandbox {} has no toolbox URL", sandbox.id)))?;
        Ok(format!("{}/{}", proxy.trim_end_matches('/'), sandbox.id))
    }

    /// `POST /files/upload-v2`: `content` as the file `path`.
    pub async fn upload(
        &self,
        toolbox: &str,
        path: &str,
        content: Vec<u8>,
        within: Duration,
    ) -> Result<(), Fault> {
        let name = path.rsplit('/').next().unwrap_or(path).to_owned();
        let form =
            multipart::Form::new().part("file", multipart::Part::bytes(content).file_name(name));
        let request = self
            .request(Method::POST, &format!("{toolbox}/files/upload-v2"), within)
            .query(&[("path", path)])
            .multipart(form);
        send(request).await.map(drop)
    }

    /// `GET /files/info`: the size of the file `path`.
    pub async fn size(&self, toolbox: &str, path: &str, within: Duration) -> Result<u64, Fault> {
        let request = self
            .request(Method::GET, &format!("{toolbox}/files/info"), within)
            .query(&[("path", path)]);
        Ok(json::<FileInfo>(send(request).await?).await?.size)
    }

    /// `POST /files/permissions`: the file `path` given `mode`.
    pub async fn chmod(
        &self,
        toolbox: &str,
        path: &str,
        mode: u32,
        within: Duration,
    ) -> Result<(), Fault> {
        let request = self
            .request(
                Method::POST,
                &format!("{toolbox}/files/permissions"),
                within,
            )
            .query(&[("path", path), ("mode", &format!("{mode:o}"))]);
        send(request).await.map(drop)
    }

    /// `DELETE /files`: the file `path`.
    pub async fn remove(&self, toolbox: &str, path: &str, within: Duration) -> Result<(), Fault> {
        let request = self
            .request(Method::DELETE, &format!("{toolbox}/files"), within)
            .query(&[("path", path)]);
        send(request).await.map(drop)
    }
}

fn sandbox_path(id: &str, action: &str) -> String {
    let id: String = id
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect();
    if action.is_empty() {
        format!("/sandbox/{id}")
    } else {
        format!("/sandbox/{id}/{action}")
    }
}

/// Sends `request`; an answer outside 2xx is a fault, with Daytona's own message.
async fn send(request: RequestBuilder) -> Result<Response, Fault> {
    let response = request.send().await.map_err(transport)?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    let message = said(&body).unwrap_or_else(|| {
        let reason = status.canonical_reason().unwrap_or("error");
        if body.trim().is_empty() {
            reason.to_owned()
        } else {
            body
        }
    });
    let line = format!("{message} (HTTP {})", status.as_u16());
    Err(match status {
        StatusCode::NOT_FOUND => Fault::NotFound(line),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Fault::Untrusted(line),
        StatusCode::CONFLICT => Fault::Conflict(line),
        _ => Fault::Failed(line),
    })
}

/// The `message` of a Daytona error body: a string, or a list of them.
fn said(body: &str) -> Option<String> {
    match serde_json::from_str::<Value>(body).ok()?.get("message")? {
        Value::String(message) => Some(message.clone()),
        Value::Array(messages) => Some(
            messages
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("; "),
        ),
        _ => None,
    }
}

async fn json<T: DeserializeOwned>(response: Response) -> Result<T, Fault> {
    let body = response.bytes().await.map_err(transport)?;
    serde_json::from_slice(&body)
        .map_err(|e| Fault::Failed(format!("an answer that does not read: {e}")))
}

/// A request that got no answer: out of time, or Daytona not reached.
fn transport(error: reqwest::Error) -> Fault {
    let line = chain(&error);
    if error.is_timeout() {
        Fault::TimedOut(line)
    } else if error.is_connect() || error.is_request() {
        Fault::Offline(line)
    } else {
        Fault::Failed(line)
    }
}

/// An error with every cause under it, which reqwest keeps apart.
fn chain(error: &dyn std::error::Error) -> String {
    let mut line = error.to_string();
    let mut cause = error.source();
    while let Some(inner) = cause {
        line.push_str(": ");
        line.push_str(&inner.to_string());
        cause = inner.source();
    }
    line
}
