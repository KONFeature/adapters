# Steery adapters and drivers

Open adapters and drivers for [Steery](https://steery.dev). `adapters/` will hold one
adapter per agent harness. `drivers/` will hold one driver per machine provider.

**Only Step A of the Daytona driver exists today:** one Daytona sandbox runs as one Steery
machine.

```
drivers/daytona/     a Bun CLI: up, logs, status, down
```

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
cd drivers/daytona
bun install
export DAYTONA_API_KEY=…

bun src/up.ts up --name goldsky --snapshot my-team-snapshot --custody file
bun src/up.ts logs --name goldsky         # follow steery run
bun src/up.ts status --name goldsky
bun src/up.ts down --name goldsky         # stop it; its disk and key are kept
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
