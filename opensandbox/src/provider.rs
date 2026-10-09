//! OpenSandbox's published HTTP APIs: the lifecycle API (`specs/sandbox-lifecycle.yml`) and
//! the execd daemon inside each sandbox (`specs/execd-api.yaml`), and how their errors read
//! as refusals.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use reqwest::{Method, RequestBuilder, multipart};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use steery_sdk::{Error, refusal};
use url::Url;

/// The header the lifecycle API reads its key from.
const KEY_HEADER: &str = "OPEN-SANDBOX-API-KEY";
/// The port execd listens on inside every sandbox.
const EXECD_PORT: &str = "44772";
/// How long one request to OpenSandbox may take where it makes or resumes a sandbox, or
/// carries a file, inside the runtime's bound of 300 s (PROTOCOL.md §9).
pub const MAKE: Duration = Duration::from_secs(280);
/// How long one request may take for anything else, inside the runtime's 60 s.
pub const CHANGE: Duration = Duration::from_secs(55);
/// How long reaching the server may take before it is out of reach.
const CONNECT: Duration = Duration::from_secs(10);
/// The longest line a refusal carries.
const LINE: usize = 300;

/// Why a call to OpenSandbox did not succeed.
#[derive(Debug)]
pub enum Fail {
    /// An HTTP answer that is no success, and what it said.
    Http { status: u16, line: String },
    /// The server could not be reached.
    Offline(String),
    /// Anything else, in one line.
    Other(String),
    /// A refusal already decided.
    Refused(&'static str, String),
}

impl Fail {
    /// A failure of `what` from the HTTP client.
    pub fn sending(what: &str, error: &reqwest::Error) -> Self {
        let line = format!("{what}: {}", chain(error));
        if error.is_connect() || error.is_timeout() {
            Self::Offline(line)
        } else if let Some(status) = error.status() {
            Self::Http {
                status: status.as_u16(),
                line,
            }
        } else {
            Self::Other(line)
        }
    }

    /// What it says, in one line.
    pub fn said(&self) -> &str {
        match self {
            Self::Http { line, .. }
            | Self::Offline(line)
            | Self::Other(line)
            | Self::Refused(_, line) => line,
        }
    }

    /// Whether this is the server's 404.
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::Http { status: 404, .. })
    }
}

/// An error and every error under it, in one line.
pub fn chain(error: &dyn std::error::Error) -> String {
    let mut line = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let said = cause.to_string();
        if !line.contains(&said) {
            line.push_str(": ");
            line.push_str(&said);
        }
        source = cause.source();
    }
    line
}

/// A sandbox as the lifecycle API answers it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Found {
    pub id: String,
    pub status: Status,
    #[serde(default)]
    pub metadata: Option<BTreeMap<String, String>>,
    pub created_at: String,
    #[serde(default)]
    pub expires_at: Option<String>,
}

/// A sandbox's state, as the lifecycle API answers it.
#[derive(Debug, Deserialize)]
pub struct Status {
    pub state: String,
    #[serde(default)]
    pub message: Option<String>,
}

/// One page of `GET /sandboxes`.
#[derive(Deserialize)]
struct Page {
    items: Vec<Found>,
    pagination: Pagination,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pagination {
    has_next_page: bool,
}

/// Where execd answers for one sandbox, and the headers its endpoint names.
#[derive(Debug, Clone)]
pub struct Execd {
    base: Url,
    pub headers: Vec<(String, String)>,
}

impl Execd {
    /// `segments` under execd's root.
    pub fn url(&self, segments: &[&str]) -> Url {
        let mut url = self.base.clone();
        if let Ok(mut path) = url.path_segments_mut() {
            path.pop_if_empty().extend(segments);
        }
        url
    }
}

/// The settings given in `initialize`, and the client that speaks to the server.
pub struct Provider {
    /// The lifecycle API's root, ending in `/v1`.
    api: Url,
    key: Option<String>,
    http: reqwest::Client,
}

impl Provider {
    /// The provider `OPENSANDBOX_URL` and `OPENSANDBOX_API_KEY` name; why not, in one line.
    pub fn new(settings: &BTreeMap<String, String>) -> Result<Self, String> {
        let raw = settings
            .get("OPENSANDBOX_URL")
            .map(|raw| raw.trim())
            .unwrap_or_default();
        if raw.is_empty() {
            return Err("OPENSANDBOX_URL is not set".into());
        }
        let mut api =
            Url::parse(raw).map_err(|_| format!("OPENSANDBOX_URL is not a URL: {raw}"))?;
        if api.scheme() != "http" && api.scheme() != "https" {
            return Err(format!("OPENSANDBOX_URL is not http or https: {raw}"));
        }
        let path = api.path().trim_end_matches('/').to_owned();
        api.set_path(&if path.ends_with("/v1") {
            path
        } else {
            format!("{path}/v1")
        });
        api.set_query(None);
        api.set_fragment(None);
        let key = settings
            .get("OPENSANDBOX_API_KEY")
            .map(|key| key.trim().to_owned())
            .filter(|key| !key.is_empty());
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT)
            .build()
            .map_err(|e| format!("the HTTP client: {}", chain(&e)))?;
        Ok(Self { api, key, http })
    }

    /// The refusal `fail` is; `otherwise` where it is none of not found, auth or network.
    pub fn refuse(&self, fail: Fail, otherwise: &'static str) -> Error {
        let (word, line) = match fail {
            Fail::Http { status: 404, line } => (refusal::NOT_FOUND, line),
            Fail::Http {
                status: 401 | 403,
                line,
            } => (refusal::UNTRUSTED, line),
            Fail::Http { line, .. } | Fail::Other(line) => (otherwise, line),
            Fail::Offline(line) => (refusal::OFFLINE, line),
            Fail::Refused(word, line) => (word, line),
        };
        Error::refused(word, self.line(&line))
    }

    /// One line for a person, never holding the API key.
    pub fn line(&self, said: &str) -> String {
        let mut text = said.to_owned();
        if let Some(key) = &self.key {
            text = text.replace(key.as_str(), "[redacted]");
        }
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let text = format!("OpenSandbox: {text}");
        if text.chars().count() > LINE {
            let mut cut: String = text.chars().take(LINE - 1).collect();
            cut.push('…');
            cut
        } else {
            text
        }
    }

    /// The lifecycle API's `segments`.
    fn url(&self, segments: &[&str]) -> Url {
        let mut url = self.api.clone();
        if let Ok(mut path) = url.path_segments_mut() {
            path.pop_if_empty().extend(segments);
        }
        url
    }

    /// Send `request`; its body, or its failure as `what`.
    async fn send(&self, request: RequestBuilder, what: &str) -> Result<Vec<u8>, Fail> {
        let response = request.send().await.map_err(|e| Fail::sending(what, &e))?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|e| Fail::sending(what, &e))?;
        if status.is_success() {
            Ok(body.to_vec())
        } else {
            Err(Fail::Http {
                status: status.as_u16(),
                line: failure(status.as_u16(), &body, what),
            })
        }
    }

    /// A lifecycle API request, its key added.
    fn lifecycle(&self, verb: Method, segments: &[&str]) -> RequestBuilder {
        let request = self
            .http
            .request(verb, self.url(segments))
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(MAKE);
        match &self.key {
            Some(key) => request.header(KEY_HEADER, key),
            None => request,
        }
    }

    pub async fn get(&self, id: &str) -> Result<Found, Fail> {
        let what = "get sandbox";
        read(
            what,
            &self
                .send(self.lifecycle(Method::GET, &["sandboxes", id]), what)
                .await?,
        )
    }

    /// Every sandbox whose metadata holds all of `metadata`, page by page.
    pub async fn listed(&self, metadata: &BTreeMap<String, String>) -> Result<Vec<Found>, Fail> {
        let what = "list sandboxes";
        // The filter is one parameter of `key=value` pairs, each encoded, and then encoded
        // whole as the parameter's value.
        let filter = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(metadata)
            .finish();
        let mut found = Vec::new();
        for page in 1.. {
            let request = self.lifecycle(Method::GET, &["sandboxes"]).query(&[
                ("metadata", filter.as_str()),
                ("page", &page.to_string()),
                ("pageSize", "200"),
            ]);
            let answer: Page = read(what, &self.send(request, what).await?)?;
            let last = !answer.pagination.has_next_page || answer.items.is_empty();
            found.extend(answer.items);
            if last {
                break;
            }
        }
        Ok(found)
    }

    pub async fn create(&self, body: &Value) -> Result<Found, Fail> {
        let what = "create sandbox";
        let request = self.lifecycle(Method::POST, &["sandboxes"]).json(body);
        read(what, &self.send(request, what).await?)
    }

    pub async fn patch_metadata(&self, id: &str, patch: &Value) -> Result<(), Fail> {
        let request = self
            .lifecycle(Method::PATCH, &["sandboxes", id, "metadata"])
            .json(patch)
            .timeout(CHANGE);
        self.send(request, "patch sandbox metadata").await.map(drop)
    }

    /// `POST /sandboxes/{id}/{action}`: `pause` or `resume`.
    pub async fn act(&self, id: &str, action: &str) -> Result<(), Fail> {
        let request = self.lifecycle(Method::POST, &["sandboxes", id, action]);
        self.send(request, &format!("{action} sandbox"))
            .await
            .map(drop)
    }

    pub async fn delete(&self, id: &str) -> Result<(), Fail> {
        let request = self
            .lifecycle(Method::DELETE, &["sandboxes", id])
            .timeout(CHANGE);
        self.send(request, "delete sandbox").await.map(drop)
    }

    /// Set the sandbox's expiration to `expires_at` (RFC 3339).
    pub async fn renew(&self, id: &str, expires_at: &str) -> Result<(), Fail> {
        let request = self
            .lifecycle(Method::POST, &["sandboxes", id, "renew-expiration"])
            .json(&json!({ "expiresAt": expires_at }))
            .timeout(CHANGE);
        self.send(request, "renew sandbox").await.map(drop)
    }

    /// Where the sandbox's execd answers: its endpoint for port 44772, with the headers it
    /// names.
    pub async fn execd(&self, id: &str) -> Result<Execd, Fail> {
        #[derive(Deserialize)]
        struct Endpoint {
            endpoint: String,
            #[serde(default)]
            headers: Option<HashMap<String, String>>,
        }
        let what = "get execd endpoint";
        let request = self
            .lifecycle(Method::GET, &["sandboxes", id, "endpoints", EXECD_PORT])
            .timeout(CHANGE);
        let endpoint: Endpoint = read(what, &self.send(request, what).await?)?;
        let raw = endpoint.endpoint.trim();
        let raw = if raw.starts_with("http://") || raw.starts_with("https://") {
            raw.to_owned()
        } else {
            format!("{}://{raw}", self.api.scheme())
        };
        let base = Url::parse(&raw)
            .map_err(|_| Fail::Other(format!("{what}: the endpoint is not a URL: {raw}")))?;
        let mut headers: Vec<(String, String)> =
            endpoint.headers.unwrap_or_default().into_iter().collect();
        headers.sort();
        Ok(Execd { base, headers })
    }

    /// An execd request, the endpoint's headers added.
    fn execd_request(&self, execd: &Execd, verb: Method, segments: &[&str]) -> RequestBuilder {
        execd
            .headers
            .iter()
            .fold(
                self.http.request(verb, execd.url(segments)),
                |request, (name, value)| request.header(name, value),
            )
            .timeout(CHANGE)
    }

    /// Write `content` to `path` with `mode` (`0o644`), through `/files/upload`.
    pub async fn upload(
        &self,
        execd: &Execd,
        path: &str,
        content: Vec<u8>,
        mode: u32,
    ) -> Result<(), Fail> {
        let what = "upload file";
        // execd reads `mode` as octal digits written in decimal: 0o644 is 644.
        let octal: u32 = format!("{:o}", mode & 0o7777).parse().unwrap_or(0);
        let metadata = json!({ "path": path, "mode": octal }).to_string();
        let name = path
            .rsplit('/')
            .find(|part| !part.is_empty())
            .unwrap_or("file")
            .to_owned();
        let form = multipart::Form::new()
            .part(
                "metadata",
                multipart::Part::text(metadata)
                    .file_name("metadata.json")
                    .mime_str("application/json")
                    .map_err(|e| Fail::Other(format!("{what}: {e}")))?,
            )
            .part(
                "file",
                multipart::Part::bytes(content)
                    .file_name(name)
                    .mime_str("application/octet-stream")
                    .map_err(|e| Fail::Other(format!("{what}: {e}")))?,
            );
        let request = self
            .execd_request(execd, Method::POST, &["files", "upload"])
            .multipart(form)
            .timeout(MAKE);
        self.send(request, what).await.map(drop)
    }

    /// The size of the file at `path`, as execd reads it.
    pub async fn size(&self, execd: &Execd, path: &str) -> Result<u64, Fail> {
        #[derive(Deserialize)]
        struct Info {
            size: u64,
        }
        let what = "read file info";
        let request = self
            .execd_request(execd, Method::GET, &["files", "info"])
            .query(&[("path", path)]);
        let infos: HashMap<String, Info> = read(what, &self.send(request, what).await?)?;
        let info = match infos.get(path) {
            Some(info) => Some(info),
            None if infos.len() == 1 => infos.values().next(),
            None => None,
        };
        info.map(|info| info.size)
            .ok_or_else(|| Fail::Other(format!("{what}: execd named no {path}")))
    }

    /// Remove the file at `path`.
    pub async fn remove_file(&self, execd: &Execd, path: &str) -> Result<(), Fail> {
        let request = self
            .execd_request(execd, Method::DELETE, &["files"])
            .query(&[("path", path)]);
        self.send(request, "remove file").await.map(drop)
    }

    /// Open a pipe-mode PTY session running `command`; its id.
    pub async fn open_pty(&self, execd: &Execd, command: &str) -> Result<String, Fail> {
        #[derive(Deserialize)]
        struct Made {
            #[serde(default)]
            session_id: Option<String>,
        }
        let what = "open pty session";
        let request = self
            .execd_request(execd, Method::POST, &["pty"])
            .json(&json!({ "command": command }));
        let made: Made = read(what, &self.send(request, what).await?)?;
        made.session_id
            .filter(|id| !id.is_empty())
            .ok_or_else(|| Fail::Other(format!("{what}: execd answered no session id")))
    }

    /// End the PTY session `session`, and whatever it still runs.
    pub async fn delete_pty(&self, execd: &Execd, session: &str) -> Result<(), Fail> {
        let request = self.execd_request(execd, Method::DELETE, &["pty", session]);
        self.send(request, "delete pty session").await.map(drop)
    }
}

/// `body` read as `T`, or why not as `what`.
fn read<T: DeserializeOwned>(what: &str, body: &[u8]) -> Result<T, Fail> {
    serde_json::from_slice(body).map_err(|e| Fail::Other(format!("{what}: unreadable answer: {e}")))
}

/// A failed answer as one line: the API's `{code, message}` where it gives one.
fn failure(status: u16, body: &[u8], what: &str) -> String {
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    let said = match serde_json::from_str::<Value>(text) {
        Ok(parsed) => {
            let inner = match parsed.get("detail") {
                Some(detail) if detail.is_object() => detail,
                _ => &parsed,
            };
            let parts: Vec<&str> = ["code", "message"]
                .iter()
                .filter_map(|name| inner.get(name).and_then(Value::as_str))
                .filter(|part| !part.is_empty())
                .collect();
            if parts.is_empty() {
                parsed
                    .get("detail")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            } else {
                parts.join(": ")
            }
        }
        Err(_) => text.to_owned(),
    };
    if said.is_empty() {
        format!("{what}: HTTP {status}")
    } else {
        format!("{what}: HTTP {status} {said}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(url: &str, key: Option<&str>) -> Result<Provider, String> {
        let mut settings = BTreeMap::from([("OPENSANDBOX_URL".to_owned(), url.to_owned())]);
        if let Some(key) = key {
            settings.insert("OPENSANDBOX_API_KEY".into(), key.into());
        }
        Provider::new(&settings)
    }

    #[test]
    fn the_api_root_ends_in_v1() {
        for (given, root) in [
            ("http://localhost:8080", "http://localhost:8080/v1"),
            ("http://localhost:8080/", "http://localhost:8080/v1"),
            ("https://box.example/v1/", "https://box.example/v1"),
            (
                "https://box.example/api?x=1#y",
                "https://box.example/api/v1",
            ),
        ] {
            assert_eq!(provider(given, None).unwrap().api.as_str(), root);
        }
        let api = provider("http://localhost:8080", None).unwrap();
        assert_eq!(
            api.url(&["sandboxes", "a/b"]).as_str(),
            "http://localhost:8080/v1/sandboxes/a%2Fb"
        );
    }

    #[test]
    fn a_url_that_is_none_is_refused() {
        assert!(Provider::new(&BTreeMap::new()).is_err());
        assert!(provider(" ", None).is_err());
        assert!(provider("localhost:8080", None).is_err());
        assert!(provider("ftp://localhost", None).is_err());
    }

    #[test]
    fn errors_read_as_refusals_without_the_key() {
        let api = provider("http://localhost:8080", Some("sekrit")).unwrap();
        let word = |fail: Fail| match api.refuse(fail, refusal::REFUSED) {
            Error::Refused { word, line } => {
                assert!(!line.unwrap_or_default().contains("sekrit"));
                word
            }
            other => panic!("{other:?}"),
        };
        let http = |status| Fail::Http {
            status,
            line: "bad sekrit".into(),
        };
        assert_eq!(word(http(404)), refusal::NOT_FOUND);
        assert_eq!(word(http(401)), refusal::UNTRUSTED);
        assert_eq!(word(http(403)), refusal::UNTRUSTED);
        assert_eq!(word(http(500)), refusal::REFUSED);
        assert_eq!(word(Fail::Offline("sekrit".into())), refusal::OFFLINE);
        assert_eq!(word(Fail::Other("x".into())), refusal::REFUSED);
    }
}
