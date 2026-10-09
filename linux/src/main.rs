//! Steery's official `linux` adapter. It provides `processes`: it starts, finds, inspects
//! and signals processes on the computer it runs on, and plans the systemd user unit that
//! keeps the runtime running there. It reads everything from `/proc`.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CStr;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{LazyLock, OnceLock};
use std::time::Duration;

use serde_json::Value;
use steery_sdk::wire::{self, process};
use steery_sdk::{Error, PROTOCOL, Program, method, refusal};

#[tokio::main(flavor = "current_thread")]
async fn main() -> io::Result<()> {
    steery_sdk::serve(|_notifier| Linux::default()).await
}

/// The adapter. It keeps only the person's home, given in `initialize`.
#[derive(Default)]
struct Linux {
    home: OnceLock<PathBuf>,
}

impl Program for Linux {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        if !params.protocol.contains(&PROTOCOL) {
            return Err(Error::invalid(format!(
                "this adapter speaks protocol {PROTOCOL} only"
            )));
        }
        let _ = self.home.set(params.home.clone());
        let facts = wire::ProcessFacts {
            os: "linux".into(),
            service: "systemd".into(),
        };
        Ok(wire::Initialized {
            protocol: PROTOCOL,
            program: wire::ProgramInfo {
                id: "linux".into(),
                kind: wire::Kind::Machine,
                release: env!("CARGO_PKG_VERSION").into(),
                label: "Linux".into(),
                short: None,
            },
            levels: Vec::new(),
            harness: None,
            provides: Some(wire::Provides::Processes),
            facts: Some(serde_json::to_value(facts).map_err(|e| Error::invalid(e.to_string()))?),
            settings: Vec::new(),
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        match method {
            method::PROCESS_START => steery_sdk::result(start(steery_sdk::params(params)?).await?),
            method::PROCESS_LIST => steery_sdk::result(list(steery_sdk::params(params)?)),
            method::PROCESS_ENVIRON => steery_sdk::result(environ(steery_sdk::params(params)?)?),
            method::PROCESS_FILES => steery_sdk::result(files(steery_sdk::params(params)?)?),
            method::PROCESS_SIGNAL => steery_sdk::result(signal(steery_sdk::params(params)?)),
            method::SERVICE_PLAN => {
                steery_sdk::result(self.service_plan(steery_sdk::params(params)?)?)
            }
            _ => Err(Error::Unknown),
        }
    }
}

/// Starts `argv` detached, in a session and process group of its own, so it outlives
/// this adapter and the runtime. With a `scope` and a reachable user systemd, it runs in
/// a transient scope unit; `systemd-run --scope` execs the command in place, so the pid
/// is the command's own.
async fn start(start: process::Start) -> Result<process::Started, Error> {
    if start.argv.is_empty() {
        return Err(Error::invalid("argv is empty"));
    }
    let argv = match start.scope.as_deref() {
        Some(scope) if user_systemd().await => {
            let mut argv = vec![
                "systemd-run".to_owned(),
                "--user".into(),
                "--scope".into(),
                "--quiet".into(),
                "--collect".into(),
                "--unit".into(),
                format!("{scope}-{}", random_hex()),
                "--".into(),
            ];
            argv.extend(start.argv);
            argv
        }
        _ => start.argv,
    };
    let failed = |what: &str, path: &Path, e: io::Error| {
        Error::refused(
            refusal::START_FAILED,
            format!("{what} {}: {e}", path.display()),
        )
    };
    let stdin = match &start.stdin {
        Some(path) => Stdio::from(File::open(path).map_err(|e| failed("stdin", path, e))?),
        None => Stdio::null(),
    };
    let stdout = append(&start.stdout).map_err(|e| failed("stdout", &start.stdout, e))?;
    let stderr = append(&start.stderr).map_err(|e| failed("stderr", &start.stderr, e))?;
    let mut command = tokio::process::Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .envs(&start.env)
        .current_dir(&start.cwd)
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr);
    // SAFETY: setsid is async-signal-safe, and the closure touches nothing else.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|e| Error::refused(refusal::START_FAILED, format!("{}: {e}", argv[0])))?;
    let pid = child
        .id()
        .ok_or_else(|| Error::refused(refusal::START_FAILED, "it exited at once"))?;
    // Read before the reaper runs: until it is reaped, an exited child keeps its entry.
    let process_start = stat(pid)
        .map(|stat| stat.start)
        .ok_or_else(|| Error::refused(refusal::START_FAILED, "its start time is unreadable"))?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(process::Started { pid, process_start })
}

/// A file opened for appending, made with mode 0600 if missing.
fn append(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

/// Whether `systemd-run` is on `PATH` and this user's systemd answers.
async fn user_systemd() -> bool {
    let on_path = std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| dir.join("systemd-run").is_file())
    });
    if !on_path {
        return false;
    }
    let output = tokio::process::Command::new("systemctl")
        .args(["--user", "is-system-running"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(Duration::from_secs(2), output).await {
        Ok(Ok(output)) => {
            output.status.success() || String::from_utf8_lossy(&output.stdout).trim() == "degraded"
        }
        _ => false,
    }
}

/// Eight random hex digits, for a scope unit's name.
fn random_hex() -> String {
    let mut bytes = [0u8; 4];
    // SAFETY: the buffer is valid for its length.
    let read = unsafe { libc::getrandom(bytes.as_mut_ptr().cast(), bytes.len(), 0) };
    if read != bytes.len() as isize {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or_default();
        bytes = (nanos ^ std::process::id()).to_le_bytes();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Every process in `/proc`, or those of `pids` that exist. A process that ends while
/// it is read is left out.
fn list(list: process::List) -> process::Listed {
    let pids: Vec<u32> = match list.pids {
        Some(pids) => pids,
        None => fs::read_dir("/proc")
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
            .collect(),
    };
    let mut users = BTreeMap::new();
    let processes = pids
        .into_iter()
        .filter_map(|pid| entry(pid, &mut users))
        .collect();
    process::Listed { processes }
}

fn entry(pid: u32, users: &mut BTreeMap<u32, String>) -> Option<process::Entry> {
    let stat = stat(pid)?;
    let command = fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let cmdline = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let uid: u32 = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    let cmdline = cmdline.strip_suffix(&[0]).unwrap_or(&cmdline);
    let args = if cmdline.is_empty() {
        Vec::new()
    } else {
        cmdline
            .split(|byte| *byte == 0)
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect()
    };
    let user = users.entry(uid).or_insert_with(|| user_name(uid)).clone();
    Some(process::Entry {
        pid,
        ppid: stat.ppid,
        process_start: stat.start,
        command: command.trim_end_matches('\n').to_owned(),
        args,
        user,
    })
}

/// The user's name from the password database, else the number.
fn user_name(uid: u32) -> String {
    let mut buffer = vec![0u8; 1024];
    loop {
        // SAFETY: passwd is plain data; getpwuid_r fills it or leaves `found` null.
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found = std::ptr::null_mut();
        // SAFETY: every pointer is valid for the length given.
        let code = unsafe {
            libc::getpwuid_r(
                uid,
                &mut entry,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut found,
            )
        };
        if code == libc::ERANGE && buffer.len() < 1 << 20 {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        if code != 0 || found.is_null() || entry.pw_name.is_null() {
            return uid.to_string();
        }
        // SAFETY: pw_name points into `buffer`, NUL-terminated.
        return unsafe { CStr::from_ptr(entry.pw_name) }
            .to_string_lossy()
            .into_owned();
    }
}

/// The asked variables of a process, refused `not-found` unless the pid still has the
/// given start time.
fn environ(environ: process::Environ) -> Result<process::Values, Error> {
    same(environ.pid, environ.process_start)?;
    let bytes = fs::read(format!("/proc/{}/environ", environ.pid))
        .map_err(|e| Error::refused(refusal::NOT_FOUND, format!("its environment: {e}")))?;
    let names: BTreeSet<&str> = environ.names.iter().map(String::as_str).collect();
    let mut values = BTreeMap::new();
    for pair in bytes.split(|byte| *byte == 0) {
        let pair = String::from_utf8_lossy(pair);
        if let Some((name, value)) = pair.split_once('=')
            && names.contains(name)
            && !values.contains_key(name)
        {
            values.insert(name.to_owned(), value.to_owned());
        }
    }
    same(environ.pid, environ.process_start)?;
    Ok(process::Values { values })
}

/// The files a process holds open: each descriptor that names an absolute path.
fn files(files: process::Files) -> Result<process::Paths, Error> {
    same(files.pid, files.process_start)?;
    let fds = fs::read_dir(format!("/proc/{}/fd", files.pid))
        .map_err(|e| Error::refused(refusal::NOT_FOUND, format!("its descriptors: {e}")))?;
    let paths: BTreeSet<PathBuf> = fds
        .flatten()
        .filter_map(|fd| fs::read_link(fd.path()).ok())
        .filter(|path| path.is_absolute())
        .collect();
    same(files.pid, files.process_start)?;
    Ok(process::Paths {
        paths: paths.into_iter().collect(),
    })
}

/// Sends the signal only where the pid still has the given start time, read right
/// before. It never signals pid 1 or below, this adapter or its parent. A group signal
/// goes to the group only when the pid leads it.
fn signal(signal: process::Signal) -> process::Sent {
    let not_sent = process::Sent { sent: false };
    let me = std::process::id();
    // SAFETY: getppid takes nothing and cannot fail.
    let parent = unsafe { libc::getppid() } as u32;
    let Ok(pid) = libc::pid_t::try_from(signal.pid) else {
        return not_sent;
    };
    if pid <= 1 || signal.pid == me || signal.pid == parent {
        return not_sent;
    }
    if same(signal.pid, signal.process_start).is_err() {
        return not_sent;
    }
    let number = match signal.signal {
        process::SignalName::Int => libc::SIGINT,
        process::SignalName::Term => libc::SIGTERM,
        process::SignalName::Kill => libc::SIGKILL,
        process::SignalName::Cont => libc::SIGCONT,
    };
    // SAFETY: plain system calls on a checked pid.
    let code = unsafe {
        if signal.group && libc::getpgid(pid) == pid {
            libc::killpg(pid, number)
        } else {
            libc::kill(pid, number)
        }
    };
    process::Sent { sent: code == 0 }
}

impl Linux {
    /// A systemd user unit that runs the binary, restarted when it fails, and the
    /// commands that load and start it.
    fn service_plan(&self, plan: process::ServicePlan) -> Result<process::Plan, Error> {
        let label = &plan.label;
        if label.is_empty()
            || !label
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
        {
            return Err(Error::invalid(format!("{label:?} is not a unit name")));
        }
        let home = match self.home.get() {
            Some(home) => home.clone(),
            None => std::env::var_os("HOME")
                .map(PathBuf::from)
                .ok_or_else(|| Error::invalid("no home"))?,
        };
        let binary = plan
            .binary
            .to_str()
            .ok_or_else(|| Error::invalid("the binary's path is not UTF-8"))?;
        let exec = std::iter::once(binary)
            .chain(plan.args.iter().map(String::as_str))
            .map(exec_word)
            .collect::<Vec<_>>()
            .join(" ");
        let environment: String = plan
            .env
            .iter()
            .map(|(name, value)| format!("Environment={}\n", assignment(name, value)))
            .collect();
        let content = format!(
            "[Unit]\n\
             Description={description}\n\n\
             [Service]\n\
             Type=simple\n\
             ExecStart={exec}\n\
             {environment}\
             Restart=on-failure\n\
             KillMode=process\n\n\
             [Install]\n\
             WantedBy=default.target\n",
            description = label.replace('%', "%%"),
        );
        Ok(process::Plan {
            files: vec![process::PlannedFile {
                path: home.join(format!(".config/systemd/user/{label}.service")),
                content,
                mode: 0o644,
            }],
            commands: vec![
                vec!["systemctl".into(), "--user".into(), "daemon-reload".into()],
                vec![
                    "systemctl".into(),
                    "--user".into(),
                    "enable".into(),
                    "--now".into(),
                    format!("{label}.service"),
                ],
            ],
        })
    }
}

/// One `Environment=` value: quoted, with what systemd reads inside quotes written so it
/// is read as itself, as the runtime writes its own unit.
fn assignment(name: &str, value: &str) -> String {
    quoted(&format!("{name}={value}"), false)
}

/// One word of `ExecStart=`: quoted like an assignment, and `$` doubled, since systemd
/// expands variables there.
fn exec_word(word: &str) -> String {
    quoted(word, true)
}

fn quoted(text: &str, dollars: bool) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '\n' => out.push_str("\\n"),
            '$' if dollars => out.push_str("$$"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// What `/proc/<pid>/stat` gives this adapter.
struct Stat {
    ppid: u32,
    /// Milliseconds since 1970.
    start: u64,
}

/// Reads fields 4 (ppid) and 22 (start time in clock ticks since boot), counted after
/// the last `)`, since the command name may hold spaces and parentheses.
fn stat(pid: u32) -> Option<Stat> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &text[text.rfind(')')? + 1..];
    // `rest` starts at field 3.
    let mut fields = rest.split_whitespace();
    let ppid = fields.nth(1)?.parse().ok()?;
    let ticks: u64 = fields.nth(17)?.parse().ok()?;
    let (boot, tick) = (*CLOCK)?;
    Some(Stat {
        ppid,
        start: boot + ticks * 1000 / tick,
    })
}

/// The boot time in milliseconds since 1970 (`btime` of `/proc/stat`) and the clock
/// ticks per second.
static CLOCK: LazyLock<Option<(u64, u64)>> = LazyLock::new(|| {
    let text = fs::read_to_string("/proc/stat").ok()?;
    let boot: u64 = text
        .lines()
        .find_map(|line| line.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()?;
    // SAFETY: sysconf only reads a constant.
    let tick = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    (tick > 0).then_some((boot * 1000, tick as u64))
});

/// Refused `not-found` unless the pid exists with a start time within
/// `SAME_START_MS` of the given one.
fn same(pid: u32, process_start: u64) -> Result<(), Error> {
    match stat(pid) {
        Some(stat) if stat.start.abs_diff(process_start) <= process::SAME_START_MS => Ok(()),
        Some(_) => Err(Error::refused(
            refusal::NOT_FOUND,
            format!("pid {pid} is another process now"),
        )),
        None => Err(Error::refused(
            refusal::NOT_FOUND,
            format!("pid {pid} is gone"),
        )),
    }
}
