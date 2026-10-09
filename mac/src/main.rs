//! Steery's official `mac` adapter. It provides `processes`: it starts, finds, inspects
//! and signals processes on the Mac it runs on, and plans the launchd agent that keeps the
//! runtime running there. It reads processes through libproc and `sysctl`.
//!
//! The work itself is macOS's alone ([`macos`]). Built for another system, the adapter
//! still says what it is, and refuses every method.

#[cfg(target_os = "macos")]
mod macos;

use std::io;
use std::path::PathBuf;
use std::sync::OnceLock;

use serde_json::Value;
use steery_sdk::wire;
use steery_sdk::{Error, PROTOCOL, Program};

#[tokio::main(flavor = "current_thread")]
async fn main() -> io::Result<()> {
    steery_sdk::serve(|_notifier| Mac::default()).await
}

/// The adapter. It keeps only the person's home, given in `initialize`.
#[derive(Default)]
struct Mac {
    home: OnceLock<PathBuf>,
}

impl Program for Mac {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        if !params.protocol.contains(&PROTOCOL) {
            return Err(Error::invalid(format!(
                "this adapter speaks protocol {PROTOCOL} only"
            )));
        }
        let _ = self.home.set(params.home.clone());
        let facts = wire::ProcessFacts {
            os: "macos".into(),
            service: "launchd".into(),
        };
        Ok(wire::Initialized {
            protocol: PROTOCOL,
            program: wire::ProgramInfo {
                id: "mac".into(),
                kind: wire::Kind::Machine,
                release: env!("CARGO_PKG_VERSION").into(),
                label: "macOS".into(),
                short: None,
            },
            levels: Vec::new(),
            harness: None,
            provides: Some(wire::Provides::Processes),
            facts: Some(serde_json::to_value(facts).map_err(|e| Error::invalid(e.to_string()))?),
            settings: Vec::new(),
        })
    }

    #[cfg(target_os = "macos")]
    async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        use steery_sdk::method;
        match method {
            method::PROCESS_START => {
                steery_sdk::result(macos::start(steery_sdk::params(params)?).await?)
            }
            method::PROCESS_LIST => steery_sdk::result(macos::list(steery_sdk::params(params)?)),
            method::PROCESS_ENVIRON => {
                steery_sdk::result(macos::environ(steery_sdk::params(params)?)?)
            }
            method::PROCESS_FILES => steery_sdk::result(macos::files(steery_sdk::params(params)?)?),
            method::PROCESS_SIGNAL => {
                steery_sdk::result(macos::signal(steery_sdk::params(params)?))
            }
            method::SERVICE_PLAN => {
                let home = match self.home.get() {
                    Some(home) => home.clone(),
                    None => std::env::var_os("HOME")
                        .map(PathBuf::from)
                        .ok_or_else(|| Error::invalid("no home"))?,
                };
                steery_sdk::result(macos::service_plan(&home, steery_sdk::params(params)?)?)
            }
            _ => Err(Error::Unknown),
        }
    }

    /// Off macOS there is nothing this adapter can do.
    #[cfg(not(target_os = "macos"))]
    async fn call(&self, _method: &str, _params: Value) -> Result<Value, Error> {
        Err(Error::refused(
            steery_sdk::refusal::REFUSED,
            "the mac adapter runs on macOS only",
        ))
    }
}
