//! Steery's official `macos` adapter. It provides `processes`: it starts, finds, inspects
//! and signals processes on the Mac it runs on, and keeps the runtime running there as a
//! launchd agent. It reads processes through libproc and `sysctl`.
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
        Ok(wire::Initialized {
            protocol: PROTOCOL,
            program: wire::ProgramInfo {
                id: "macos".parse().map_err(Error::invalid)?,
                kind: wire::Kind::Machine,
                release: env!("CARGO_PKG_VERSION").into(),
                label: "macOS".into(),
                short: None,
            },
            harness: None,
            machine: Some(wire::Machine::Processes(wire::ProcessFacts {
                os: wire::Os::Macos,
                service: wire::ServiceManager::Launchd,
            })),
            settings: Vec::new(),
        })
    }

    #[cfg(target_os = "macos")]
    async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        use steery_sdk::{method, params as given, result};
        match method {
            method::PROCESS_START => result(macos::start(given(params)?)?),
            method::PROCESS_LIST => result(macos::list(given(params)?)),
            method::PROCESS_ENVIRON => result(macos::environ(given(params)?)?),
            method::PROCESS_FILES => result(macos::files(given(params)?)?),
            method::PROCESS_LISTENING => result(macos::listening(given(params)?).await?),
            method::PROCESS_SIGNAL => result(macos::signal(given(params)?)),
            method::SERVICE_PLAN => result(macos::service_plan(&self.home()?, given(params)?)?),
            method::SERVICE_STATUS => {
                result(macos::service_status(&self.home()?, given(params)?).await?)
            }
            method::SERVICE_START => {
                result(macos::service_start(&self.home()?, given(params)?, false).await?)
            }
            method::SERVICE_RESTART => {
                result(macos::service_start(&self.home()?, given(params)?, true).await?)
            }
            method::SERVICE_REMOVE => {
                result(macos::service_remove(&self.home()?, given(params)?).await?)
            }
            _ => Err(Error::Unknown),
        }
    }

    /// Off macOS there is nothing this adapter can do.
    #[cfg(not(target_os = "macos"))]
    async fn call(&self, _method: &str, _params: Value) -> Result<Value, Error> {
        Err(Error::refused(
            steery_sdk::refusal::REFUSED,
            "the macos adapter runs on macOS only",
        ))
    }
}

#[cfg(target_os = "macos")]
impl Mac {
    /// The person's home: the one `initialize` gave, else `HOME`.
    fn home(&self) -> Result<PathBuf, Error> {
        match self.home.get() {
            Some(home) => Ok(home.clone()),
            None => std::env::var_os("HOME")
                .map(PathBuf::from)
                .ok_or_else(|| Error::invalid("no home")),
        }
    }
}
