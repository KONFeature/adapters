# Steery adapters

Open adapters for [Steery](https://steery.dev). An adapter connects Steery to one kind of
machine: an operating system (`mac`, `linux`) or a sandbox provider (`daytona`,
`opensandbox`). Drivers, one per agent harness, live in `steery-dev/drivers`.

Two adapters exist today: `linux`, and the first step of `daytona`, where one Daytona
sandbox runs as one Steery machine.

```
linux/       a Rust binary: the official Linux adapter
daytona/     a Bun CLI: up, logs, status, down
```

## linux

`linux/` provides `processes` on Linux: it starts turns detached (in a systemd user scope
when one is reachable), lists processes and reads their environment and open files from
`/proc`, signals only a process whose start time still matches, and plans the systemd
user unit that keeps the runtime running. It ships with `steery` and updates with it.
It speaks the program protocol over standard input and output through `steery-sdk`.

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
