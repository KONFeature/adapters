# Steery adapters

Open adapters for [Steery](https://steery.dev). An adapter connects Steery to one kind of
machine: an operating system (`mac`, `linux`) or a sandbox provider (`daytona`,
`opensandbox`). Drivers, one per agent harness, live in `steery-dev/drivers`.

Four adapters exist today: `mac`, `linux`, `daytona`, which provides Daytona sandboxes,
and `opensandbox`, which provides OpenSandbox sandboxes; beside `daytona`, Step A's `up`
runs one Daytona sandbox as one Steery machine.

```
mac/         a Rust binary: the official macOS adapter
linux/       a Rust binary: the official Linux adapter
daytona/     a Node program: the Daytona adapter; and a Bun CLI: up, logs, status, down
opensandbox/ a Node program: the OpenSandbox adapter
```

## linux

`linux/` provides `processes` on Linux: it starts turns detached (in a systemd user scope
when one is reachable), lists processes and reads their environment and open files from
`/proc`, signals only a process whose start time still matches, and plans the systemd
user unit that keeps the runtime running. It ships with `steery` and updates with it.
It speaks the program protocol over standard input and output through `steery-sdk`.

## mac

`mac/` provides `processes` on macOS: it starts turns detached in a session of their own,
lists processes through libproc and reads their arguments and environment through
`sysctl` (`KERN_PROCARGS2`) and their open files through `proc_pidfdinfo`, signals only a
process whose start time still matches, and plans the LaunchAgent that keeps the runtime
running (`~/Library/LaunchAgents/<label>.plist`, logs in `~/Library/Logs/<label>.log`),
loaded with `launchctl bootstrap gui/<uid>`. launchd has no user scopes, so a start's
`scope` is ignored. macOS lets a user read only its own processes' details, so another
user's processes are left out unless it runs as root. It ships with `steery` and updates
with it. Built for another system it answers `initialize` and refuses every method.

## daytona

`daytona/` provides `sandboxes` on [Daytona](https://daytona.io): `sandboxes/list`,
`sandbox/ensure` (finds the sandbox labelled `steery-key=<key>`, or makes one labelled
`steery=sandbox` from the snapshot or image the runtime names, or Daytona's default),
`sandbox/start`, `stop`, `delete`, `archive`, `keepalive` (refreshes its last activity),
`put` (uploads a file, then sets its mode) and `channel`: an `ssh -T` to Daytona's SSH
gateway with a 60-minute access token, running the runtime's command in the sandbox. It
passes no image, environment, secret or network setting of its own, and leaves Daytona's
auto-stop as the sandbox's idle timer. Its filesystem survives a stop.

Its settings, given once on the card the runtime raises when it is added:

| Setting | | |
|---|---|---|
| `DAYTONA_API_KEY` | secret, required | an API key from <https://app.daytona.io/dashboard/keys> |
| `DAYTONA_API_URL` | optional | your Daytona API URL, for a self-hosted Daytona; absent: `https://app.daytona.io/api` |
| `DAYTONA_TARGET` | optional | the region sandboxes are made in; absent: your organization's default |

It reads them from `initialize` only, never from its environment or a `.env` file. Build it
with `npm run build` in `daytona/`; the program is `dist/adapter.js`
(`steery-adapter-daytona`), and `steery adapter test node daytona/dist/adapter.js` checks
it. `up` (Step A, below) remains, for running the whole runtime inside one sandbox.

## opensandbox

`opensandbox/` provides `sandboxes` on [OpenSandbox](https://github.com/opensandbox-group/OpenSandbox),
through its lifecycle API and the execd daemon in each sandbox, with `fetch` and Node's
`WebSocket` and no provider SDK:

- `sandboxes/list`: the sandboxes whose metadata holds `steery=sandbox` and every label asked.
- `sandbox/ensure`: finds the sandbox whose metadata holds `steery-key=<key>`, or makes one
  from the image (`image.uri`) or snapshot (`snapshotId`) the runtime names. OpenSandbox
  has no default image, so with neither it refuses `no-place`. A sandbox made from an image
  is given the entrypoint `tail -f /dev/null`, because the API requires one; its resource
  limits are left empty, so the server's own apply. Metadata keys and values follow
  Kubernetes label rules (63 characters; letters, digits, `-`, `_`, `.`), so a key or label
  outside them is refused by the server.
- `sandbox/start` resumes a paused sandbox and waits until it runs; `stop` and `pause`
  pause it (OpenSandbox's only stop that keeps it); `delete` deletes it. Its filesystem
  survives a pause.
- `sandbox/keepalive` renews the expiration by the TTL the server gave it when it was made,
  or by an hour; a sandbox with no expiration needs none. The adapter sets no timeout of its
  own, so the server's default is the sandbox's idle timer.
- `sandbox/put` uploads the file through execd (`/files/upload`) with its mode.
- `sandbox/channel` is a `stream` channel: it opens a pipe-mode PTY session in execd running
  the command (`POST /pty`) and joins its WebSocket (`/pty/{id}/ws?pty=0`), reached through
  the sandbox's endpoint for port 44772 and the headers that endpoint names.
  `stream/write` sends standard-input frames; standard output comes back as `stream/data`,
  standard error goes to the adapter's log, and `stream/closed` carries the exit code.
  execd has no frame that closes only standard input, so `stream/close` sends the command
  SIGHUP, ends the session after two seconds if it still runs, and closes the socket.

Errors read as `not-found` (HTTP 404), `untrusted` (401, 403), `offline` (the server cannot
be reached), `start-failed` (a sandbox that could not be made or resumed) or `refused`.

Its settings, given once on the card the runtime raises when it is added:

| Setting | | |
|---|---|---|
| `OPENSANDBOX_URL` | required | your OpenSandbox server, as its SDKs take it: `http://localhost:8080` |
| `OPENSANDBOX_API_KEY` | secret, optional | the server's `server.api_key`, where it has one |

It reads them from `initialize` only. Build it with `npm install && npm run build` in
`opensandbox/`; the program is `dist/adapter.js` (`steery-adapter-opensandbox`), and
`steery adapter test node opensandbox/dist/adapter.js` checks it.

## What Steery adds to a sandbox

- The `steery` binary.
- `steery setup <harness>`, which writes the harness's MCP entry, skill and hooks, as on a
  laptop.
- `steery run`, the runtime.

It adds nothing else. The sandbox is yours: your snapshot, your Daytona settings, your
harness, set up the way you already set them up.

## The one line for your image

In a Dockerfile:

```dockerfile
RUN curl -fsSL https://steery.dev/install.sh | sh -s -- --install-only --to /usr/local/bin
```

In Daytona's declarative builder:

```ts
.runCommands('curl -fsSL https://steery.dev/install.sh | sh -s -- --install-only --to /usr/local/bin')
```

Without it, `up` installs the binary into `$HOME/.local/bin` in the sandbox the first time
it starts.

## Step A: a Daytona sandbox as a Steery machine

### What you need

- **A Daytona API key** in `DAYTONA_API_KEY` (create one at
  <https://app.daytona.io/dashboard/keys>).
- **A sandbox that can reach <https://steery.dev>.** The runtime inside talks to Steery's
  server.
- **The harness installed in your snapshot**, the way you install your other tools. `up`
  stops and prints the harness's own installer if it is missing.
- **You, at your terminal and with your phone.** Setup's sign-in code is approved on the
  phone, and you compare each signer's fingerprint with the one the phone shows before you
  trust it.
- **[Bun](https://bun.sh)** on your computer.

### Run it

```sh
cd daytona
bun install
export DAYTONA_API_KEY=…

bun src/up.ts up --name team --snapshot my-team-snapshot --custody file
bun src/up.ts logs --name team         # follow steery run
bun src/up.ts status --name team
bun src/up.ts down --name team         # stop it; its disk and key are kept
```

`up` creates the sandbox `steery-<name>` from `--snapshot <name>` or `--image <ref>`. With
neither, Daytona uses its default snapshot. If the sandbox exists, `up` finds it by its
labels (`steery=machine`, `name=<name>`) and starts it. Auto-stop is off, because the
runtime lives in the sandbox and the sandbox is the machine.

Then `up` installs `steery` if it is missing, and checks that the harness is installed
(`--harness omp`, `claude-code` or `codex`; `omp` by default). It runs
`steery setup <harness> --name daytona-<name> --no-service` in a terminal inside the
sandbox, joined to yours. Nothing is pinned for you; `--trust <fingerprint>` names a signer
you already compared. Last, it starts `steery run` as a long-running Daytona session
command. The machine shows in Steery as `daytona-<name>`.

A sandbox has no TPM, so the machine's reader key can only be kept in a file there.
Anything that can read that file can read every answer sent to this machine. `--custody
file` is your consent to that; setup uses a file only when you say so.

## License

MIT
