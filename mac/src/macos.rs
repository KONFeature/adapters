//! The adapter's work on macOS: libproc and `sysctl` for processes, launchd for the
//! service. The kernel lets a user read only its own processes' details, so a process of
//! another user is left out of a list and refused like a gone one; root reads them all.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CStr, OsString};
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::size_of;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::ptr;
use std::sync::LazyLock;
use std::time::Duration;

use libc::c_int;
use steery_sdk::wire::{self, process};
use steery_sdk::{Error, refusal};
use tokio::time::Instant;

/// `PROC_PIDFDVNODEPATHINFO` of `sys/proc_info.h`, which libc does not declare.
const PROC_PIDFDVNODEPATHINFO: c_int = 2;

/// `struct vnode_fdinfowithpath` of `sys/proc_info.h`, which libc does not declare.
#[repr(C)]
struct VnodeFdInfoWithPath {
    /// `struct proc_fileinfo`: 24 bytes, 8-aligned, this adapter does not read.
    _file: [u64; 3],
    path: libc::vnode_info_path,
}

/// Starts `argv` detached, in a session and process group of its own, with exactly the
/// environment `env`, so it outlives this adapter and the runtime: launchd stops only the
/// stopped job's own process group. `scope` is ignored: launchd has no share of the CPU
/// to run it in.
pub async fn start(start: process::Start) -> Result<process::Started, Error> {
    let Some((program, args)) = start.argv.split_first() else {
        return Err(Error::invalid("argv is empty"));
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
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .env_clear()
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
        .map_err(|e| Error::refused(refusal::START_FAILED, format!("{program}: {e}")))?;
    let pid = child
        .id()
        .ok_or_else(|| Error::refused(refusal::START_FAILED, "it exited at once"))?;
    // Read before the reaper runs: until it is reaped, an exited child keeps its entry.
    let process_start = info(pid)
        .map(|info| info.start)
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

/// Every process running, or those of `pids` that run, as [`entry`] reads each.
pub fn list(list: process::List) -> process::Listed {
    let pids = match list.pids {
        Some(pids) => pids,
        None => all_pids(),
    };
    let mut users = BTreeMap::new();
    let mut buffer = Vec::new();
    let processes = pids
        .into_iter()
        .filter_map(|pid| entry(pid, &mut users, &mut buffer))
        .collect();
    process::Listed { processes }
}

/// Every pid running, from `proc_listallpids`.
fn all_pids() -> Vec<u32> {
    // SAFETY: a null buffer asks only for the count.
    let count = unsafe { libc::proc_listallpids(ptr::null_mut(), 0) };
    let Ok(count) = usize::try_from(count) else {
        return Vec::new();
    };
    // Room for processes started since the count.
    let mut pids: Vec<libc::pid_t> = vec![0; count + 64];
    let Ok(bytes) = c_int::try_from(pids.len() * size_of::<libc::pid_t>()) else {
        return Vec::new();
    };
    // SAFETY: the buffer is valid for `bytes`.
    let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    let Ok(count) = usize::try_from(count) else {
        return Vec::new();
    };
    pids.truncate(count);
    pids.into_iter()
        .filter_map(|pid| u32::try_from(pid).ok())
        .collect()
}

/// One process: left out where it is a zombie, gone, or another user's whose arguments
/// this user may not read; one of this user's whose arguments cannot be read is listed
/// without them. Root reads every process's, and lists every process. `kernel_task`
/// (pid 0) is no process to name.
fn entry(
    pid: u32,
    users: &mut BTreeMap<u32, String>,
    buffer: &mut Vec<u8>,
) -> Option<process::Entry> {
    if pid == 0 {
        return None;
    }
    let info = info(pid).filter(|info| !info.zombie)?;
    let args = procargs(pid, buffer).map(|procargs| procargs.args);
    if args.is_none() && info.uid != *ME && *ME != 0 {
        return None;
    }
    let user = users
        .entry(info.uid)
        .or_insert_with(|| user_name(info.uid))
        .clone();
    Some(process::Entry {
        pid,
        ppid: info.ppid,
        process_start: info.start,
        command: info.command,
        args,
        user,
        cwd: cwd(pid),
    })
}

/// The folder a process works in now; none where it is gone or this user may not read it.
fn cwd(pid: u32) -> Option<PathBuf> {
    let pid = c_int::try_from(pid).ok()?;
    // SAFETY: proc_vnodepathinfo is plain data.
    let mut paths: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = size_of::<libc::proc_vnodepathinfo>() as c_int;
    // SAFETY: the buffer is valid for `size` bytes.
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            (&raw mut paths).cast(),
            size,
        )
    };
    if read != size {
        return None;
    }
    let path = PathBuf::from(OsString::from_vec(bytes_of(
        paths.pvi_cdir.vip_path.as_flattened(),
    )));
    path.is_absolute().then_some(path)
}

/// The user this adapter runs as.
static ME: LazyLock<u32> = LazyLock::new(|| {
    // SAFETY: getuid takes nothing and cannot fail.
    unsafe { libc::getuid() }
});

/// The user's name from the password database, else the number.
fn user_name(uid: u32) -> String {
    let mut buffer = vec![0u8; 1024];
    loop {
        // SAFETY: passwd is plain data; getpwuid_r fills it or leaves `found` null.
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found = ptr::null_mut();
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
pub fn environ(environ: process::Environ) -> Result<process::Values, Error> {
    same(environ.pid, environ.process_start)?;
    let mut buffer = Vec::new();
    let procargs = procargs(environ.pid, &mut buffer).ok_or_else(|| {
        Error::refused(
            refusal::NOT_FOUND,
            format!("pid {}'s environment is unreadable", environ.pid),
        )
    })?;
    let names: BTreeSet<&str> = environ.names.iter().map(String::as_str).collect();
    let mut values = BTreeMap::new();
    for pair in &procargs.environ {
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

/// The files a process holds open: each vnode descriptor that names an absolute path.
pub fn files(files: process::Files) -> Result<process::Paths, Error> {
    same(files.pid, files.process_start)?;
    let unreadable = || {
        Error::refused(
            refusal::NOT_FOUND,
            format!(
                "pid {}'s descriptors: {}",
                files.pid,
                io::Error::last_os_error()
            ),
        )
    };
    let pid = c_int::try_from(files.pid).map_err(|_| unreadable())?;
    let size = size_of::<libc::proc_fdinfo>();
    // SAFETY: a null buffer asks only for the size.
    let bytes = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, ptr::null_mut(), 0) };
    let bytes = usize::try_from(bytes).map_err(|_| unreadable())?;
    // Room for descriptors opened since the size.
    // SAFETY: proc_fdinfo is plain data.
    let mut fds: Vec<libc::proc_fdinfo> = vec![unsafe { std::mem::zeroed() }; bytes / size + 16];
    let room = c_int::try_from(fds.len() * size).map_err(|_| unreadable())?;
    // SAFETY: the buffer is valid for `room` bytes.
    let bytes =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), room) };
    let bytes = usize::try_from(bytes).map_err(|_| unreadable())?;
    fds.truncate(bytes / size);
    let mut paths = BTreeSet::new();
    for fd in fds {
        if fd.proc_fdtype != libc::PROX_FDTYPE_VNODE as u32 {
            continue;
        }
        // SAFETY: the struct is plain data.
        let mut vnode: VnodeFdInfoWithPath = unsafe { std::mem::zeroed() };
        let size = size_of::<VnodeFdInfoWithPath>() as c_int;
        // SAFETY: the buffer is valid for `size` bytes.
        let read = unsafe {
            libc::proc_pidfdinfo(
                pid,
                fd.proc_fd,
                PROC_PIDFDVNODEPATHINFO,
                (&raw mut vnode).cast(),
                size,
            )
        };
        // A descriptor closed since the list is left out.
        if read != size {
            continue;
        }
        let path = PathBuf::from(OsString::from_vec(bytes_of(
            vnode.path.vip_path.as_flattened(),
        )));
        if path.is_absolute() {
            paths.insert(path);
        }
    }
    same(files.pid, files.process_start)?;
    Ok(process::Paths {
        paths: paths.into_iter().collect(),
    })
}

/// Sends the signal only where the pid still has the given start time, read right
/// before. It never signals pid 1 or below, this adapter or its parent. A group signal
/// goes to the group only when the pid leads it.
pub fn signal(signal: process::Signal) -> process::Sent {
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

/// The seconds the runtime takes to stop once asked: its own shutdown grace, which its
/// plist gives launchd before it kills it.
const STOP_GRACE_SECS: u64 = 40;

/// The longest `process/listening` waits on `lsof`, inside the runtime's 5 s bound.
const LISTENING_BOUND: Duration = Duration::from_secs(4);

/// The longest `service/status` waits on launchd, inside the runtime's 10 s bound.
const STATUS_BOUND: Duration = Duration::from_secs(8);

/// The longest `service/start`, `restart` and `remove` wait on launchd, inside the
/// runtime's 30 s bound.
const SERVICE_BOUND: Duration = Duration::from_secs(28);

/// The processes listening on a TCP port, as `lsof` finds them. macOS shows a user only
/// its own processes' sockets unless it runs as root.
pub async fn listening(listening: process::Listening) -> Result<process::Listeners, Error> {
    let deadline = Instant::now() + LISTENING_BOUND;
    let port = format!("-iTCP:{}", listening.port);
    let output = output(
        "/usr/sbin/lsof",
        &["-nP", &port, "-sTCP:LISTEN", "-Fp"],
        deadline,
    )
    .await
    .map_err(refused)?;
    // `lsof` exits 1 when nothing listens; each process it found is a `p<pid>` line.
    let pids: BTreeSet<u32> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix('p')?.parse().ok())
        .collect();
    let processes = pids
        .into_iter()
        .filter_map(|pid| {
            let info = info(pid).filter(|info| !info.zombie)?;
            Some(process::Listener {
                pid,
                process_start: info.start,
                command: info.command,
            })
        })
        .collect();
    Ok(process::Listeners { processes })
}

/// A LaunchAgent, as the runtime installs itself: it runs the binary at login and keeps
/// it running, handed its listener by launchd on 127.0.0.1:`port`, at a lower priority
/// (macOS bounds a user's CPU no other way, so `cpu` is checked and has nothing to set),
/// its output appended to `log`. It keeps every directory of the `PATH` the plist on
/// disk gave it. The commands unload the job an earlier plist loaded, which fails where
/// none is and may, enable it, which an earlier `launchctl disable` would otherwise keep
/// from loading, and load it in this user's GUI domain.
pub fn service_plan(home: &Path, plan: process::ServicePlan) -> Result<process::Plan, Error> {
    let label = launchd_label(&plan.label)?;
    if !(1..=100).contains(&plan.cpu) {
        return Err(Error::invalid(format!(
            "cpu is a percent from 1 to 100, not {}",
            plan.cpu
        )));
    }
    if plan.port == 0 {
        return Err(Error::invalid("port 0 is no address to listen on"));
    }
    let binary = absolute_utf8(&plan.binary, "the binary")?;
    let log = absolute_utf8(&plan.log, "the log")?;
    let home_text = absolute_utf8(home, "the home")?;
    let path = plist(home, label);
    let mut env = plan.env;
    let search = search_path(env.get("PATH").map(String::as_str), &path, binary);
    env.insert("PATH".into(), search);
    let arguments: String = std::iter::once(binary)
        .chain(plan.args.iter().map(String::as_str))
        .map(|word| format!("    <string>{}</string>\n", escape(word)))
        .collect();
    let environment: String = env
        .iter()
        .map(|(name, value)| {
            format!(
                "    <key>{}</key><string>{}</string>\n",
                escape(name),
                escape(value)
            )
        })
        .collect();
    let content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key><array>
{arguments}  </array>
  <key>WorkingDirectory</key><string>{home}</string>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>ExitTimeOut</key><integer>{STOP_GRACE_SECS}</integer>
  <key>Nice</key><integer>10</integer>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
  <key>EnvironmentVariables</key><dict>
{environment}  </dict>
  <key>Sockets</key><dict>
    <key>Listeners</key><dict>
      <key>SockNodeName</key><string>127.0.0.1</string>
      <key>SockServiceName</key><string>{port}</string>
      <key>SockType</key><string>stream</string>
      <key>SockFamily</key><string>IPv4</string>
    </dict>
  </dict>
</dict></plist>
"#,
        home = escape(home_text),
        log = escape(log),
        port = plan.port,
    );
    let target = target(label);
    let launchctl = |args: &[&str], may_fail: bool| process::PlanCommand {
        argv: std::iter::once("launchctl")
            .chain(args.iter().copied())
            .map(str::to_owned)
            .collect(),
        may_fail,
    };
    let commands = vec![
        launchctl(&["bootout", &target], true),
        launchctl(&["enable", &target], false),
        launchctl(&["bootstrap", &domain(), &path.to_string_lossy()], false),
    ];
    Ok(process::Plan {
        files: vec![process::PlannedFile {
            path,
            content,
            mode: 0o644,
        }],
        commands,
    })
}

/// Whether the agent's plist is on disk, whether launchd runs it and as which process,
/// and the CPU this Mac grants the user: all its online cores.
pub async fn service_status(
    home: &Path,
    service: process::Service,
) -> Result<process::Status, Error> {
    let label = launchd_label(&service.label)?;
    let installed = plist(home, label).is_file();
    let deadline = Instant::now() + STATUS_BOUND;
    let pid = match printed(&target(label), deadline).await {
        Ok(Some(printed)) => printed.lines().find_map(|line| {
            line.trim()
                .strip_prefix("pid = ")?
                .parse::<u32>()
                .ok()
                .filter(|pid| *pid != 0)
        }),
        Ok(None) => None,
        // launchd could not be asked, and nothing of the agent's is on disk.
        Err(_) if !installed => None,
        Err(line) => return Err(refused(line)),
    };
    let process_start = pid
        .and_then(info)
        .filter(|info| !info.zombie)
        .map(|info| info.start);
    // SAFETY: sysconf reads a constant of the running system.
    let online = u32::try_from(unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) })
        .ok()
        .filter(|n| *n > 0)
        .unwrap_or(1);
    Ok(process::Status {
        installed,
        running: process_start.is_some(),
        pid: process_start.and(pid),
        process_start,
        granted: online.saturating_mul(100),
    })
}

/// Has launchd run the agent: `kickstart` where its job is loaded (`-k` stops it first,
/// for a restart), else loads it from its plist, which runs it at load. launchd stops only
/// a job's own process group, so the turns it started in theirs keep running.
pub async fn service_start(
    home: &Path,
    service: process::Service,
    restart: bool,
) -> Result<wire::Empty, Error> {
    let label = launchd_label(&service.label)?;
    let target = target(label);
    let deadline = Instant::now() + SERVICE_BOUND;
    if loaded(&target, deadline).await.map_err(refused)? {
        let args: &[&str] = if restart {
            &["kickstart", "-k", &target]
        } else {
            &["kickstart", &target]
        };
        run("launchctl", args, deadline).await.map_err(refused)?;
        return Ok(wire::Empty {});
    }
    let path = plist(home, label);
    if !path.is_file() {
        return Err(refused(format!(
            "{label} is not installed: {} is not there",
            path.display()
        )));
    }
    run("launchctl", &["enable", &target], deadline)
        .await
        .map_err(refused)?;
    run(
        "launchctl",
        &["bootstrap", &domain(), &path.to_string_lossy()],
        deadline,
    )
    .await
    .map_err(refused)?;
    Ok(wire::Empty {})
}

/// Removes the plist, then unloads the job, as the runtime uninstalls itself: the file
/// first, since unloading stops the runtime that may be asking, and a plist left on disk
/// is one launchd loads again at the next login. A job that is gone by the time it is
/// unloaded is unloaded.
pub async fn service_remove(home: &Path, service: process::Service) -> Result<wire::Empty, Error> {
    let label = launchd_label(&service.label)?;
    let path = plist(home, label);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(refused(format!("removing {}: {e}", path.display()))),
    }
    let target = target(label);
    let deadline = Instant::now() + SERVICE_BOUND;
    if !loaded(&target, deadline).await.map_err(refused)? {
        return Ok(wire::Empty {});
    }
    if let Err(line) = run("launchctl", &["bootout", &target], deadline).await
        && loaded(&target, deadline).await.map_err(refused)?
    {
        return Err(refused(line));
    }
    Ok(wire::Empty {})
}

/// `label` as a launchd label: letters, digits and `-_.`.
fn launchd_label(label: &str) -> Result<&str, Error> {
    let fits = !label.is_empty()
        && label.len() <= 200
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if fits {
        Ok(label)
    } else {
        Err(Error::invalid(format!("{label:?} is not a launchd label")))
    }
}

/// The agent's plist.
fn plist(home: &Path, label: &str) -> PathBuf {
    home.join(format!("Library/LaunchAgents/{label}.plist"))
}

/// This user's GUI domain.
fn domain() -> String {
    // SAFETY: geteuid takes nothing and cannot fail.
    format!("gui/{}", unsafe { libc::geteuid() })
}

/// The agent's job in this user's GUI domain.
fn target(label: &str) -> String {
    format!("{}/{label}", domain())
}

/// `path` as UTF-8 text, refused unless it is absolute.
fn absolute_utf8<'a>(path: &'a Path, what: &str) -> Result<&'a str, Error> {
    if !path.is_absolute() {
        return Err(Error::invalid(format!("{what}'s path is not absolute")));
    }
    path.to_str()
        .ok_or_else(|| Error::invalid(format!("{what}'s path is not UTF-8")))
}

/// The `PATH` the agent runs with: the one asked for (else this adapter's own, which is
/// the runtime's), then every directory the plist on disk at `plist` gave it, then the
/// binary's own directory, each once, in that order, so an install never narrows it.
fn search_path(asked: Option<&str>, plist: &Path, binary: &str) -> String {
    let own = std::env::var("PATH").unwrap_or_default();
    let installed = std::fs::read_to_string(plist)
        .ok()
        .and_then(|body| plist_path(&body))
        .unwrap_or_default();
    let beside = Path::new(binary)
        .parent()
        .and_then(Path::to_str)
        .unwrap_or_default();
    let mut kept: Vec<&str> = Vec::new();
    for dir in asked
        .unwrap_or(&own)
        .split(':')
        .chain(installed.split(':'))
        .chain(std::iter::once(beside))
    {
        if !dir.is_empty() && !kept.contains(&dir) {
            kept.push(dir);
        }
    }
    kept.join(":")
}

/// The `PATH` string of a plist's environment, read back as [`escape`] wrote it.
fn plist_path(body: &str) -> Option<String> {
    const PATH: &str = "<key>PATH</key><string>";
    let value = &body[body.find(PATH)? + PATH.len()..];
    let value = &value[..value.find("</string>")?];
    Some(
        value
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&"),
    )
}

/// Whether launchd has `target` loaded: `print` answers for a loaded job and says
/// `Could not find service` for one that is not; anything else is no answer.
async fn loaded(target: &str, deadline: Instant) -> Result<bool, String> {
    printed(target, deadline)
        .await
        .map(|printed| printed.is_some())
}

/// What `launchctl print` says of a loaded `target`; `None` where it is not loaded.
async fn printed(target: &str, deadline: Instant) -> Result<Option<String>, String> {
    let args = ["print", target];
    let output = output("launchctl", &args, deadline).await?;
    if output.status.success() {
        return Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()));
    }
    if String::from_utf8_lossy(&output.stderr).contains("Could not find service") {
        return Ok(None);
    }
    Err(failed("launchctl", &args, &output))
}

/// Runs `program` with `args` to its end before `deadline`, refused unless it succeeds,
/// with one line saying how it failed: launchd's own words where it said any.
async fn run(program: &str, args: &[&str], deadline: Instant) -> Result<(), String> {
    let output = output(program, args, deadline).await?;
    if output.status.success() {
        Ok(())
    } else {
        Err(failed(program, args, &output))
    }
}

/// Runs `program` with `args` to its end before `deadline`, whatever its exit status.
async fn output(program: &str, args: &[&str], deadline: Instant) -> Result<Output, String> {
    let command = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout_at(deadline, command).await {
        Err(_) => Err(format!(
            "`{program} {}` did not finish in time",
            args.join(" ")
        )),
        Ok(Err(e)) => Err(format!("`{program} {}`: {e}", args.join(" "))),
        Ok(Ok(output)) => Ok(output),
    }
}

/// How a command failed, in one line: its standard error, or its exit status where it
/// said nothing.
fn failed(program: &str, args: &[&str], output: &Output) -> String {
    let text = String::from_utf8_lossy(&output.stderr);
    let said = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    let said = if said.is_empty() {
        output.status.to_string()
    } else {
        said
    };
    format!("`{program} {}` failed: {said}", args.join(" "))
}

/// Refused `refused` with launchd's line.
fn refused(line: String) -> Error {
    Error::refused(refusal::REFUSED, line)
}

/// The five characters an XML document cannot carry raw. Every value written into the
/// plist is somebody's path or setting, so none of them is assumed to be safe.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// What `proc_pidinfo(PROC_PIDTBSDINFO)` gives this adapter.
struct Info {
    ppid: u32,
    /// The effective user.
    uid: u32,
    /// It exited and waits to be reaped.
    zombie: bool,
    /// Milliseconds since 1970.
    start: u64,
    command: String,
}

/// A process's BSD info; none where it is gone or this user may not read it.
fn info(pid: u32) -> Option<Info> {
    let pid = c_int::try_from(pid).ok()?;
    // SAFETY: proc_bsdinfo is plain data.
    let mut bsd: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = size_of::<libc::proc_bsdinfo>() as c_int;
    // SAFETY: the buffer is valid for `size` bytes.
    let read =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut bsd).cast(), size) };
    if read != size {
        return None;
    }
    // `pbi_name` is the longer name, as `proc_name` reads it; `pbi_comm` when it is empty.
    let name = if bsd.pbi_name[0] != 0 {
        &bsd.pbi_name[..]
    } else {
        &bsd.pbi_comm[..]
    };
    Some(Info {
        ppid: bsd.pbi_ppid,
        uid: bsd.pbi_uid,
        zombie: bsd.pbi_status == libc::SZOMB,
        start: bsd.pbi_start_tvsec * 1000 + bsd.pbi_start_tvusec / 1000,
        command: String::from_utf8_lossy(&bytes_of(name)).into_owned(),
    })
}

/// A C string's bytes, up to its first NUL.
fn bytes_of(chars: &[libc::c_char]) -> Vec<u8> {
    chars
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect()
}

/// What `KERN_PROCARGS2` gives this adapter.
struct Procargs {
    /// The arguments, its own name first.
    args: Vec<String>,
    /// The environment, `NAME=value` each.
    environ: Vec<String>,
}

/// A process's arguments and environment, read into `buffer`; none where it is gone or
/// this user may not read it. The kernel lays them out as `argc`, the executable's path
/// and NULs to align, then each argument and each variable NUL-terminated, and an empty
/// string after the environment.
fn procargs(pid: u32, buffer: &mut Vec<u8>) -> Option<Procargs> {
    let mut mib = [
        libc::CTL_KERN,
        libc::KERN_PROCARGS2,
        c_int::try_from(pid).ok()?,
    ];
    buffer.resize(*ARGMAX, 0);
    let mut size = buffer.len();
    // SAFETY: the name has three parts, and the buffer is valid for `size` bytes.
    let code = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buffer.as_mut_ptr().cast(),
            &mut size,
            ptr::null_mut(),
            0,
        )
    };
    if code != 0 {
        return None;
    }
    let bytes = buffer.get(..size)?;
    let argc = usize::try_from(i32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?)).ok()?;
    let rest = &bytes[4..];
    let rest = &rest[rest.iter().position(|b| *b == 0)?..];
    let rest = &rest[rest.iter().position(|b| *b != 0).unwrap_or(rest.len())..];
    let mut strings = rest.split(|b| *b == 0);
    let lossy = |text: &[u8]| String::from_utf8_lossy(text).into_owned();
    let args = strings.by_ref().take(argc).map(lossy).collect();
    let environ = strings
        .take_while(|text| !text.is_empty())
        .map(lossy)
        .collect();
    Some(Procargs { args, environ })
}

/// The kernel's bound on a process's arguments and environment together (`KERN_ARGMAX`).
static ARGMAX: LazyLock<usize> = LazyLock::new(|| {
    let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    let mut value: c_int = 0;
    let mut size = size_of::<c_int>();
    // SAFETY: the name has two parts, and `value` is valid for `size` bytes.
    let code = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            (&raw mut value).cast(),
            &mut size,
            ptr::null_mut(),
            0,
        )
    };
    match usize::try_from(value) {
        Ok(value) if code == 0 && value > 0 => value,
        _ => 1 << 20,
    }
});

/// Refused `not-found` unless the pid runs, no zombie, with a start time within
/// `SAME_START_MS` of the given one.
fn same(pid: u32, process_start: u64) -> Result<(), Error> {
    match info(pid).filter(|info| !info.zombie) {
        Some(info) if info.start.abs_diff(process_start) <= process::SAME_START_MS => Ok(()),
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
