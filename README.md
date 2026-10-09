# Steery adapters

An adapter connects [Steery](https://steery.dev) to one kind of machine: an operating system
(`macos`, `linux`) or a sandbox provider (`daytona`, `opensandbox`). The Steery runtime
starts an adapter as a child process and speaks the program protocol to it, JSON-RPC 2.0
over standard input and output (`specs/PROTOCOL.md` in Steery's repository). Drivers, one
per agent harness, live in `steery-dev/drivers`.

Each adapter is its own program, `steery-adapter-<id>`, built on the `steery-sdk` crate,
and answers version 1 of the program protocol. The four below are official: they ship in
Steery's release archive, beside `steery`, each built from this repository at the commit
`programs.lock` in Steery's repository names, and update with it; an operating-system
adapter ships only in that system's archives. Its id is the name Rust gives that system
(`std::env::consts::OS`), and the runtime runs the one named for the system it runs on.

```
macos/       steery-adapter-macos: processes on macOS
linux/       steery-adapter-linux: processes on Linux
daytona/     steery-adapter-daytona: Daytona sandboxes
opensandbox/ steery-adapter-opensandbox: OpenSandbox sandboxes
```

Each crate is a Cargo workspace of its own, with its own `Cargo.lock`, so it builds alone.
What `linux` and `macos` share, where POSIX behaves the same on both, is the
[`common`](common/) crate, `steery-adapter-common`, each naming it by path (`../common`):
`process/start`'s detached start with its `STEERY_STARTED` and status file, the output
files it appends to, and a user's name. What each system keeps — `/proc` or libproc,
systemd or launchd — stays in its own adapter.

## Building

The SDK and the protocol live in Steery's own repository. Every crate names the SDK by path,
`steery-sdk = { path = "../../server/crates/steery-sdk" }`, so check Steery's repository out
beside this one as `server`:

```sh
git clone https://github.com/steery-dev/server
git clone https://github.com/steery-dev/adapters
cd adapters/linux && cargo build
```

A release builds each official adapter the same way, from its own crate, with `--locked` and
the `ship` profile of Steery's `Cargo.toml` (`deploy/compile-cli.sh` in Steery's
repository):

```sh
cargo build --profile ship --locked --config <ship profile> \
  --manifest-path linux/Cargo.toml --bin steery-adapter-linux
```

## Checking an adapter

```sh
steery adapter test target/debug/steery-adapter-linux
```

`steery adapter test <command> [args…] [--setting NAME[=VALUE]]` starts the adapter and
checks what it says it is, the methods that read and change nothing, an id sent twice, a
method it does not have, and its shutdown, one line per check; it exits 0 only when every
one passes. A read answered with a refusal word its method lists is an answer.
`--setting NAME=VALUE` passes a setting to `initialize`, and `--setting NAME` takes its
value from the environment, keeping a secret off the command line; a method that needs a
required setting not given is skipped. `daytona` and `opensandbox` also have their own
`cargo test`.

## Writing an adapter

An adapter for a machine Steery does not ship is a program like these. `linux/` is the
worked example: one file, `src/main.rs`.

- **Its crate** depends on `steery-sdk` by path as above, with `tokio` for its runtime and
  `serde_json` for the parameters, and declares an empty `[workspace]` so it builds alone.
  Its binary is named `steery-adapter-<id>`; `main` is
  `steery_sdk::serve(|_notifier| Linux::default())`. The SDK reads requests, answers each
  one, and gives the notifier for what the adapter says unasked (a `stream` channel's
  `stream/data` and `stream/closed`).
- **`initialize`** (`Program::initialize`) refuses a runtime that does not offer
  `steery_sdk::PROTOCOL`, keeps the `home` it is given, and answers `wire::Initialized`:
  `program` (`id: "linux"`, `kind: Machine`, `release`, `label: "Linux"`), no `harness`,
  `machine` (`wire::Machine::Processes` with `os: Linux` and `service: Systemd`; a sandbox
  provider answers `Sandboxes` with what it keeps, how it idles, its optional methods and
  its channel; PROTOCOL.md §3.2), and the settings it needs (none here; `daytona` and
  `opensandbox` declare theirs, §3.4).
- **The methods** (`Program::call`) are one match from each `steery_sdk::method` name to the
  function that answers it, its parameters read with `steery_sdk::params` and its answer
  given with `steery_sdk::result`: `process/*` and `service/*` for `processes` (§5.1),
  `sandboxes/list` and `sandbox/*` for `sandboxes` (§5.2), `stream/*` for a `stream`
  channel (§5.3); any other answers `Error::Unknown`. A refusal is `Error::refused` with
  one of the protocol's words (`steery_sdk::refusal`).

## Adding an adapter to an install

An official adapter ships in Steery's archive, beside `steery`. Any other is added with
`steery adapter add`:

```sh
steery adapter add <source> [--crate <dir>] [--wait-secs <n>]
steery adapter add https://github.com/steery-dev/adapters#<rev> --crate linux
```

`<source>` is an executable's path; a folder with a `Cargo.toml`, built there with
`cargo build --release --locked` (refused without a `Cargo.lock` at its workspace root, which
every crate here has); or a git repository's URL (one with `://`, or `git@host:`), with an
optional `#<rev>` (a branch, tag or commit), cloned to `<config>/programs/src/<id>` and built
the same way. `--crate` names the crate's folder inside the repository. The official ids,
`linux`, `macos`, `daytona` and `opensandbox`, are refused, so a fork of a crate here must answer `initialize` with an id of its
own.

What runs is a copy, `<config>/programs/bin/<id>-<the first 12 hex of its digest>`. Before
anything is asked, the copy is started and checked as `steery adapter test` checks it: an
official id, a kind other than an adapter's, an `initialize` that fails or speaks another
protocol, or any failed check refuses it, and nothing is written. Then one card on the
person's phone, "Add the adapter <label>", names it, its source, release, digest, what it
provides and the settings it declares. Only a verified Go writes its record,
`<config>/adapters/<id>.json`:

```json
{"id": "my-cloud", "kind": "machine",
 "command": "<config>/programs/bin/my-cloud-<12 hex>", "args": [],
 "source": "https://github.com/me/my-cloud#<commit>",
 "release": "0.1.0", "digest": "sha256:<64 lowercase hex>"}
```

`<config>` is this computer's Steery folder, `$XDG_CONFIG_HOME/steery`, else
`~/.config/steery`; every machine this computer runs reads the records there, a sandbox
machine's runtime (`steery run --machine`) too. `digest` is `sha256:` and the SHA-256 of the
file `command` names, in lowercase hex. `source` is the words that add it again: the
absolute path, the absolute folder, or `<url>#<commit> [--crate <dir>]`, pinned to the
commit built. The command exits 0 once added, 1 on No or an answer that does not verify,
and 2 with no answer within `--wait-secs` (by default and at most 3600).

The runtime checks the digest before every start: an adapter whose copy changed on disk is
not started, and reads Missing with "add it again with `steery adapter add <source>`".
`steery adapter remove <id>` deletes the record, its copy and its clone; an official id with
no record is refused.

## linux

`linux/` provides `processes` on Linux (`initialize` answers
`machine: {provides: "processes", os: "linux", service: "systemd"}`).

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

The channel runs `ssh` on the controlling computer, which carries the command's standard
input and output byte for byte, so an OpenSSH client there is a prerequisite (`apt install
openssh-client` on a bare Debian or Ubuntu). Where none is on `PATH`, `sandbox/channel` and
`sandboxes/list` — the first call `steery machine add` makes, so nothing is written — refuse
`refused`: "Daytona's channel runs `ssh` on this computer, and none is installed: install an
OpenSSH client (e.g. apt install openssh-client)".

It waits for Daytona inside the runtime's bounds: 280 s for `ensure`, `start`, `archive`
and `put` (bound 300 s), 55 s for the rest (bound 60 s), and 5 s for each file call after
an upload. Errors read as `not-found` (HTTP 404); `offline` where Daytona is down — not
reached, a gateway answering for it (HTTP 502, 503, 504), or a request with no answer in
its time — a retry the server bounds; `refused` for a refused API key (HTTP 401, 403),
whose line names the setting to give again (`Daytona refused the API key (HTTP 401: …):
give DAYTONA_API_KEY again`), since a bad key does not heal by waiting; `start-failed` (a
sandbox that could not be made or started); or `refused`, which is also a sandbox that
did not reach the state asked for in time, and a put whose upload got no answer by its
bound. Each line is Daytona's own message, without the API key.

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
DAYTONA_API_KEY=… steery adapter test target/debug/steery-adapter-daytona --setting DAYTONA_API_KEY
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

Errors read as `not-found` (HTTP 404); `offline` where OpenSandbox is down — not reached,
a gateway answering for it (HTTP 502, 503, 504), or no answer in time; `refused` for a
refused API key (HTTP 401, 403), whose line names `OPENSANDBOX_API_KEY` to give (again);
`start-failed` (a sandbox that could not be made or resumed); or `refused`.

Its settings, given once on the card the runtime raises when it is added:

| Setting | | |
|---|---|---|
| `OPENSANDBOX_URL` | required | your OpenSandbox server, as its SDKs take it: `http://localhost:8080` |
| `OPENSANDBOX_API_KEY` | secret, optional | the server's `server.api_key`, where it has one |

It reads them from `initialize` only, never from its environment. Build and check it:

```sh
cd opensandbox && cargo build && cargo clippy && cargo test
steery adapter test target/debug/steery-adapter-opensandbox \
  --setting OPENSANDBOX_URL=http://localhost:8080 --setting OPENSANDBOX_API_KEY
```

The check lists sandboxes on the server it is given; a local server is the
`opensandbox/server` image (`opensandbox/server:release-1.1.0`, which runs sandboxes with
`opensandbox/execd:v1.1.0`) with the Docker runtime, as its
`server/docker-compose.example.yaml` sets it up.

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
