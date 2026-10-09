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
use std::process::Stdio;
use std::ptr;
use std::sync::LazyLock;

use libc::c_int;
use steery_sdk::wire::process;
use steery_sdk::{Error, refusal};

/// `PROC_PIDFDVNODEPATHINFO` of `sys/proc_info.h`, which libc does not declare.
const PROC_PIDFDVNODEPATHINFO: c_int = 2;

/// `struct vnode_fdinfowithpath` of `sys/proc_info.h`, which libc does not declare.
#[repr(C)]
struct VnodeFdInfoWithPath {
    /// `struct proc_fileinfo`: 24 bytes, 8-aligned, this adapter does not read.
    _file: [u64; 3],
    path: libc::vnode_info_path,
}

/// Starts `argv` detached, in a session and process group of its own, so it outlives
/// this adapter and the runtime: launchd kills only the stopped job's own process group.
/// `scope` is ignored, since launchd has no user scopes like systemd's.
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

/// Every process, or those of `pids` that exist. A process that ends while it is read,
/// or that this user may not read, is left out.
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

fn entry(
    pid: u32,
    users: &mut BTreeMap<u32, String>,
    buffer: &mut Vec<u8>,
) -> Option<process::Entry> {
    let info = info(pid)?;
    let args = procargs(pid, buffer)?.args;
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
    })
}

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

/// A LaunchAgent that runs the binary at login, restarted when it fails, and the
/// commands that load it in this user's GUI domain, as the runtime writes its own.
pub fn service_plan(home: &Path, plan: process::ServicePlan) -> Result<process::Plan, Error> {
    let label = &plan.label;
    if label.is_empty()
        || !label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(Error::invalid(format!("{label:?} is not a launchd label")));
    }
    let binary = plan
        .binary
        .to_str()
        .ok_or_else(|| Error::invalid("the binary's path is not UTF-8"))?;
    let home_text = home
        .to_str()
        .ok_or_else(|| Error::invalid("the home's path is not UTF-8"))?;
    let path = home.join(format!("Library/LaunchAgents/{label}.plist"));
    let logs = format!("{home_text}/Library/Logs/{label}.log");
    let arguments: String = std::iter::once(binary)
        .chain(plan.args.iter().map(String::as_str))
        .map(|word| format!("    <string>{}</string>\n", escape(word)))
        .collect();
    let environment: String = plan
        .env
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
  <key>KeepAlive</key><dict>
    <key>SuccessfulExit</key><false/>
  </dict>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>Nice</key><integer>10</integer>
  <key>StandardOutPath</key><string>{logs}</string>
  <key>StandardErrorPath</key><string>{logs}</string>
  <key>EnvironmentVariables</key><dict>
{environment}  </dict>
</dict></plist>
"#,
        label = escape(label),
        home = escape(home_text),
        logs = escape(&logs),
    );
    // SAFETY: geteuid takes nothing and cannot fail.
    let uid = unsafe { libc::geteuid() };
    let domain = format!("gui/{uid}");
    Ok(process::Plan {
        files: vec![process::PlannedFile {
            path: path.clone(),
            content,
            mode: 0o644,
        }],
        commands: vec![
            // Unloads the job an earlier plan loaded, so the new plist is read. It fails
            // when no such job is loaded, as on the first install.
            vec![
                "launchctl".into(),
                "bootout".into(),
                format!("{domain}/{label}"),
            ],
            vec![
                "launchctl".into(),
                "bootstrap".into(),
                domain,
                path.to_string_lossy().into_owned(),
            ],
        ],
    })
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
    /// The real user.
    uid: u32,
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
        uid: bsd.pbi_ruid,
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

/// Refused `not-found` unless the pid exists with a start time within
/// `SAME_START_MS` of the given one.
fn same(pid: u32, process_start: u64) -> Result<(), Error> {
    match info(pid) {
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
