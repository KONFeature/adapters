# Steery adapters

Open adapters for [Steery](https://steery.dev). An adapter connects Steery to one kind of
machine: an operating system (`macos`, `linux`) or a sandbox provider (`daytona`,
`opensandbox`). Drivers, one per agent harness, live in `steery-dev/drivers`.

Four adapters exist today: `macos`, `linux`, `daytona`, which provides Daytona sandboxes,
and `opensandbox`, which provides OpenSandbox sandboxes. An operating-system adapter's id
is the name Rust gives that system (`std::env::consts::OS`): the runtime runs the one
named for the system it runs on.

```
macos/       a Rust binary: the official macOS adapter
linux/       a Rust binary: the official Linux adapter
daytona/     a Rust binary: the official Daytona adapter
opensandbox/ a Rust binary: the official OpenSandbox adapter
```

## Building

The SDKs and the protocol live in Steery's own repository until they are published. Check
it out beside this one, as `server`; every adapter uses `../server/crates/steery-sdk`.

```sh
git clone https://github.com/steery-dev/server
git clone https://github.com/steery-dev/adapters
cd adapters/linux && cargo build
steery adapter test target/debug/steery-adapter-linux
```

## linux

`linux/` provides `processes` on Linux (`initialize` answers
`machine: {provides: "processes", os: "linux", service: "systemd"}`). It ships with
`steery` and updates with it, and speaks the program protocol over standard input and
output through `steery-sdk`.

- `process/start` starts a turn detached, in a session of its own, with exactly the
  environment it is given. With a `scope` naming the service the runtime runs as under
  systemd (the runtime's cgroup is `<label>.service`), it runs in a transient scope in
  `<label>.slice` (`systemd-run --user --scope --slice=<label>.slice`), under the slice's
  CPU quota and outside the runtime's unit; anywhere else it runs as it is.
- `process/list` reads `/proc`; a zombie is not listed, and `args` is absent where `/proc`
  hides a process's command line. `process/environ` and `process/files` read `/proc`, and
  `process/signal` signals only a process whose start time still matches.
- `process/listening` finds the `LISTEN` sockets on the port in `/proc/net/tcp` and `tcp6`
  and the processes holding them under `/proc/<pid>/fd`.
- `service/plan` writes three systemd user units, as the runtime installs itself:
  `<label>.service` (the binary, `Restart=always`, `KillMode=process`, output appended to
  `log`, the `PATH` asked for merged with the one the unit on disk gave), `<label>.socket`
  (`ListenStream=127.0.0.1:<port>`, handed to the runtime) and `<label>.slice`
  (`CPUQuota` of `cpu` percent of what the computer grants). Its commands reload systemd,
  enable the socket and the service, bind the socket again only where its unit changed,
  and `loginctl enable-linger`, which may fail.
- `service/status` answers whether the unit is on disk and systemd runs it, its pid, and
  `granted`: online cores, narrowed by the CPU affinity and every cgroup v1 or v2 quota
  above the runtime's own slice, in percent of one core.
- `service/start` starts the service; `service/restart` stops it, moves what it left in
  its cgroup (turns, kept by `KillMode=process`) into a scope of their own in the slice,
  and starts it; `service/remove` disables and removes the service, then the socket, then
  the slice's file, never stopping the slice or a turn. Each refuses `refused` with
  systemd's line.

## macos

`macos/` provides `processes` on macOS (`initialize` answers
`machine: {provides: "processes", os: "macos", service: "launchd"}`). It ships with
`steery` and updates with it. Built for another system it answers `initialize` and
refuses every method.

- `process/start` starts a turn detached, in a session of its own, with exactly the
  environment it is given. launchd has no share of the CPU to run it in, so `scope` is
  ignored.
- `process/list` reads processes through libproc and their arguments and environment
  through `sysctl` (`KERN_PROCARGS2`). A zombie is not listed. One of this user's whose
  arguments cannot be read is listed without `args`; another user's is left out unless
  the adapter runs as root. `process/files` reads `proc_pidfdinfo`, and `process/signal`
  signals only a process whose start time still matches.
- `process/listening` asks `lsof -iTCP:<port> -sTCP:LISTEN`.
- `service/plan` writes the LaunchAgent `~/Library/LaunchAgents/<label>.plist`, as the
  runtime installs itself: `KeepAlive`, a lower priority, output appended to `log`, the
  `PATH` asked for merged with the one the plist on disk gave, and a `Listeners` socket on
  127.0.0.1:`port` handed to the runtime. Its commands are `launchctl bootout` (which may
  fail), `enable` and `bootstrap gui/<uid>`.
- `service/status` answers whether the plist is on disk and launchd runs the job, its pid,
  and `granted`: every online core, in percent of one core.
- `service/start` and `service/restart` `launchctl kickstart` the job (`-k` to restart),
  or load it from its plist where it is not loaded; launchd stops only the job's own
  process group, so turns keep running. `service/remove` removes the plist, then boots
  the job out. Each refuses `refused` with launchd's line.

## daytona

`daytona/` provides `sandboxes` on [Daytona](https://daytona.io) (`initialize` answers
`machine: {provides: "sandboxes", keeps: "filesystem", idle: "activity", optional:
["archive", "keepalive"], channel: "argv", os: "linux"}`). It calls Daytona's published
REST API directly, over HTTPS (rustls): the control plane
([`openapi.json`](https://www.daytona.io/docs/openapi.json)) and each sandbox's toolbox
([`toolbox-openapi.json`](https://www.daytona.io/docs/toolbox-openapi.json)), through the
toolbox proxy Daytona names for it. It passes no image, environment, secret or network
setting of its own, and leaves Daytona's auto-stop as the sandbox's idle timer.

- `sandboxes/list`: the sandboxes labelled `steery=sandbox` and every label asked, page by
  page. Daytona's listing lags a change by a second or more.
- `sandbox/ensure`: the sandbox named `steery-<first 128 bits of the key's SHA-256, hex>`,
  or one made under that name, labelled `steery=sandbox`, `steery-key=<key>` and the
  labels asked, from the snapshot or image (built `FROM` it) the runtime names, or
  Daytona's default snapshot, and waited for until it runs. Daytona keeps names unique and
  reads them back at once, so an ensure sent twice, even at once, makes one sandbox. A
  sandbox of that name without those labels is refused; one being deleted is waited out.
- `sandbox/start`, `stop`, `delete` and `archive` ask Daytona and wait until the sandbox is
  `started`, `stopped`, `destroyed` or `archived`, reading it again every second (every
  two while archiving); `error` or `build_failed` fails. Archive stops a started sandbox
  first, since Daytona archives only a stopped one. Its filesystem survives a stop and an
  archive.
- `sandbox/keepalive` refreshes the sandbox's last activity, which auto-stop counts from.
- `sandbox/put` uploads the file (`/files/upload-v2`), reads its size back
  (`/files/info`) and sets its mode (`/files/permissions`). A file that timed out or came
  out short is deleted and the put refused; a large file is better fetched from inside.
- `sandbox/channel` is an `argv` channel: `ssh -T` to Daytona's SSH gateway
  (`ssh.app.daytona.io`, or the host and port Daytona's answer names) with a 60-minute
  access token, running the runtime's command, quoted for the sandbox's shell.

It waits for Daytona inside the runtime's bounds: 280 s for `ensure`, `start`, `archive`
and `put` (bound 300 s), 55 s for the rest (bound 60 s), and 5 s for each file call after
an upload. Errors read as `not-found` (HTTP 404), `untrusted` (401, 403), `offline`
(Daytona not reached), `start-failed` (a sandbox that could not be made or started), or
`refused`, which is also every call that ran out of time; each line is Daytona's own
message, without the API key.

Its settings, given once on the card the runtime raises when it is added:

| Setting | | |
|---|---|---|
| `DAYTONA_API_KEY` | secret, required | an API key from <https://app.daytona.io/dashboard/keys> |
| `DAYTONA_API_URL` | optional | your Daytona API URL, for a self-hosted Daytona; absent: `https://app.daytona.io/api` |
| `DAYTONA_TARGET` | optional | the region sandboxes are made in; absent: your organization's default |

It reads them from `initialize` only, never from its environment or a file. Build and
check it:

```sh
cd daytona && cargo build && cargo clippy && cargo test
steery adapter test target/debug/steery-adapter-daytona --setting DAYTONA_API_KEY=…
```

With the key, the check also lists your sandboxes (`sandboxes/list`); it makes none.

## opensandbox

`opensandbox/` provides `sandboxes` on [OpenSandbox](https://github.com/opensandbox-group/OpenSandbox)
(`initialize` answers `machine: {provides: "sandboxes", keeps: "filesystem", idle: "ttl",
optional: ["pause", "keepalive"], channel: "stream", os: "linux"}`). It is a Rust binary,
`steery-adapter-opensandbox`, speaking the program protocol through `steery-sdk`, and talks
to OpenSandbox's published lifecycle API and to the execd daemon in each sandbox over HTTP
and WebSocket (rustls), with no provider SDK:

- `sandboxes/list`: the sandboxes whose metadata holds `steery=sandbox` and every label asked.
- `sandbox/ensure`: finds the sandbox whose metadata holds `steery-key=<key>`, or makes one
  from the image (`image.uri`) or snapshot (`snapshotId`) the runtime names. OpenSandbox
  has no default image, so with neither it refuses `no-place`. A sandbox made from an image
  is given the entrypoint `tail -f /dev/null`, because the API requires one; its resource
  limits are left empty, so the server's own apply. Metadata keys and values follow
  Kubernetes label rules (63 characters; letters, digits, `-`, `_`, `.`), so a key or label
  outside them is refused by the server. OpenSandbox takes no name to make a sandbox by and
  has nothing to lock on, so ensures for one key run one after the other in the adapter,
  and after making one it lists the key again: the oldest sandbox, by the creation time the
  listing reads, is the key's, and a younger one it made is deleted. Two adapters making one
  at once thus answer one sandbox, as long as the server lists each before the other's
  create answers (a create still pulling its image is not listed).
- `sandbox/start` resumes a paused sandbox and waits until it runs; `stop` and `pause`
  pause it (OpenSandbox's only stop that keeps it); `delete` deletes it. Its filesystem
  survives a pause.
- `sandbox/keepalive` renews the expiration by the TTL the server gave it when it was made
  (kept in its metadata as `steery-ttl`), or by an hour; a sandbox with no expiration needs
  none. The adapter sets no timeout of its own, so the server's default is the sandbox's
  idle timer; on the Docker runtime that default is none, and a sandbox lives until it is
  deleted.
- `sandbox/put` uploads the file through execd (`/files/upload`) with its mode, then reads
  its size back (`/files/info`); a file that is not whole is removed before it refuses.
- `sandbox/channel` is a `stream` channel: it opens a pipe-mode PTY session in execd running
  the command (`POST /pty`) and joins its WebSocket (`/pty/{id}/ws?pty=0`), reached through
  the sandbox's endpoint for port 44772 and the headers that endpoint names.
  `stream/write` sends standard-input frames; standard output comes back as `stream/data`,
  standard error goes to the adapter's log, and `stream/closed` carries the exit code (none
  for a command a signal ended).
  execd has no frame that closes only standard input, so `stream/close` sends the command
  SIGHUP, ends the session after two seconds if it still runs, and closes the socket.

Errors read as `not-found` (HTTP 404), `untrusted` (401, 403), `offline` (the server cannot
be reached), `start-failed` (a sandbox that could not be made or resumed) or `refused`.

Its settings, given once on the card the runtime raises when it is added:

| Setting | | |
|---|---|---|
| `OPENSANDBOX_URL` | required | your OpenSandbox server, as its SDKs take it: `http://localhost:8080` |
| `OPENSANDBOX_API_KEY` | secret, optional | the server's `server.api_key`, where it has one |

It reads them from `initialize` only, never from its environment. Build and check it:

```sh
cd opensandbox && cargo build && cargo clippy && cargo test
steery adapter test target/debug/steery-adapter-opensandbox \
  --setting OPENSANDBOX_URL=http://localhost:8080 --setting OPENSANDBOX_API_KEY=…
```

The check lists sandboxes on the server it is given; a local server is the
`opensandbox/server` image with the Docker runtime (its `server/docker-compose.example.yaml`).

## What Steery adds to a sandbox

A sandbox machine (`steery machine add daytona`) adds to each sandbox it uses:

- the `steery` binary and its official programs, which `steery sandbox serve` runs inside;
- the harness's MCP entry, skill and hooks, as on a laptop.

It adds nothing else. The sandbox is yours: your snapshot, your Daytona settings, your
harness, set up the way you already set them up.

Where `steery` is missing in a sandbox, Steery installs it over the channel the first
time. To have it there already, add the one line to your image. In a Dockerfile:

```dockerfile
RUN curl -fsSL https://steery.dev/install.sh | sh -s -- --install-only --to /usr/local/bin
```

In Daytona's declarative builder:

```ts
.runCommands('curl -fsSL https://steery.dev/install.sh | sh -s -- --install-only --to /usr/local/bin')
```

## License

MIT
