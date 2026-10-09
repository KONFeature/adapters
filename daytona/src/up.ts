#!/usr/bin/env bun
// One Daytona sandbox as one Steery machine (Step A).
//
//   bun src/up.ts up --name <n> [--snapshot <name> | --image <ref>] [--harness <id>]
//                    [--custody auto|file] [--trust <fingerprint>]... [--url <server>]
//   bun src/up.ts logs --name <n>
//   bun src/up.ts status --name <n>
//   bun src/up.ts down --name <n>
//
// The Daytona API key comes from DAYTONA_API_KEY; DAYTONA_API_URL and DAYTONA_TARGET
// are read by the SDK itself when set.

import { parseArgs } from 'node:util'
import { Daytona, Sandbox, SandboxState } from '@daytona/sdk'

/** The toolbox session `steery run` lives in. */
const RUN_SESSION = 'steery-run'
const SERVER = 'https://steery.dev'
/** Every command in the sandbox finds what the steery installer and the harness installers put there. */
const PATH_PREFIX = 'export PATH="$HOME/.local/bin:/usr/local/bin:$HOME/.bun/bin:$PATH"'
/** The steery installer, binary only, into $HOME/.local/bin: no sudo. */
const INSTALL_STEERY = 'curl -fsSL https://steery.dev/install.sh | sh -s -- --install-only'
/** The line a company adds to its image so `up` never installs steery itself. */
const IMAGE_LINE = 'curl -fsSL https://steery.dev/install.sh | sh -s -- --install-only --to /usr/local/bin'

/** The harnesses `up` sets up: the binary each puts on PATH, and its vendor's installer. */
const HARNESSES: Record<string, { binary: string; install: string }> = {
  omp: { binary: 'omp', install: 'curl -fsSL https://omp.sh/install | sh' },
  'claude-code': { binary: 'claude', install: 'curl -fsSL https://claude.ai/install.sh | bash' },
  codex: { binary: 'codex', install: 'npm install -g @openai/codex' },
}

const USAGE = `Usage:
  bun src/up.ts up --name <n> [--snapshot <name> | --image <ref>] [--harness <id>]
                   [--custody auto|file] [--trust <fingerprint>]... [--url <server>]
  bun src/up.ts logs --name <n>
  bun src/up.ts status --name <n>
  bun src/up.ts down --name <n>

up        creates the sandbox steery-<n> from your snapshot or image (or Daytona's
          default snapshot), or finds it by its labels and starts it. It installs the
          steery binary if the sandbox has none, runs \`steery setup <harness>\` in it at
          your terminal, then starts \`steery run\`. The machine shows in Steery as
          daytona-<n>.
logs      follows the output of \`steery run\` in the sandbox.
status    prints the sandbox's state and whether \`steery run\` is running.
down      stops the sandbox; its disk, and the machine's key on it, are kept.

--harness   the harness to set up: ${Object.keys(HARNESSES).join(', ')} (default omp). It must
            already be installed in the sandbox.
--custody   \`file\` keeps the machine's reader key in a file in the sandbox, which has no
            TPM; anything that can read that file can read every answer sent to it.
            Saying it is your consent, as \`steery setup --custody file\` says.
--trust     a signer fingerprint you compared with your phone (Settings); repeatable.
            Without it setup lists the signers at your terminal and asks.

DAYTONA_API_KEY must be set.`

class Usage extends Error {}

function fail(message: string): never {
  throw new Usage(message)
}

const { positionals, values } = parseArgs({
  args: Bun.argv.slice(2),
  allowPositionals: true,
  options: {
    name: { type: 'string' },
    snapshot: { type: 'string' },
    image: { type: 'string' },
    harness: { type: 'string' },
    custody: { type: 'string' },
    trust: { type: 'string', multiple: true },
    url: { type: 'string' },
    help: { type: 'boolean', short: 'h' },
  },
})

/** The person's name for the machine: what `steery-<n>` and `daytona-<n>` are made of. */
function machineName(): string {
  const name = values.name ?? fail('--name <n> is required')
  if (!/^[a-z0-9][a-z0-9-]{0,40}$/.test(name)) {
    fail(`--name takes lowercase letters, digits and dashes, starting with a letter or digit: ${name}`)
  }
  return name
}

function client(): Daytona {
  const apiKey = process.env.DAYTONA_API_KEY
  if (!apiKey) fail('DAYTONA_API_KEY is not set: create a key at https://app.daytona.io/dashboard/keys')
  return new Daytona({ apiKey })
}

/** One argument for `sh`, whatever it holds. */
function quote(word: string): string {
  return `'${word.replaceAll("'", `'\\''`)}'`
}

/** A shell script in the sandbox, as its own user with its own $HOME. */
function sh(sandbox: Sandbox, script: string, timeout?: number) {
  return sandbox.process.executeCommand(`sh -c ${quote(`${PATH_PREFIX} && ${script}`)}`, undefined, undefined, timeout)
}

async function find(daytona: Daytona, name: string): Promise<Sandbox | undefined> {
  for await (const sandbox of daytona.list({ labels: { steery: 'machine', name } })) {
    return sandbox
  }
  return undefined
}

/** Brings a sandbox that exists to `started`, whatever state it was left in. */
async function ensureStarted(sandbox: Sandbox): Promise<void> {
  switch (sandbox.state) {
    case SandboxState.STARTED:
      return
    case SandboxState.STOPPED:
    case SandboxState.ARCHIVED:
      console.error(`Starting ${sandbox.name} (${sandbox.state})…`)
      await sandbox.start(300)
      return
    case SandboxState.CREATING:
    case SandboxState.STARTING:
    case SandboxState.RESTORING:
    case SandboxState.PULLING_SNAPSHOT:
      await sandbox.waitUntilStarted(300)
      return
    default:
      fail(`${sandbox.name} is ${sandbox.state}${sandbox.errorReason ? `: ${sandbox.errorReason}` : ''}`)
  }
}

/** The sandbox from the company's own snapshot or image; with neither, Daytona's default snapshot. */
async function create(daytona: Daytona, name: string): Promise<Sandbox> {
  if (values.snapshot !== undefined && values.image !== undefined) {
    fail('--snapshot and --image name the same thing; give one')
  }
  const base = {
    name: `steery-${name}`,
    labels: { steery: 'machine', name },
    // The runtime lives in the sandbox, so the sandbox is the machine and must stay up.
    autoStopInterval: 0,
  }
  if (values.image !== undefined) {
    console.error(`Creating steery-${name} from the image ${values.image}…`)
    return daytona.create(
      { ...base, image: values.image },
      { timeout: 0, onSnapshotCreateLogs: (chunk) => process.stderr.write(chunk) },
    )
  }
  console.error(`Creating steery-${name} from ${values.snapshot !== undefined ? `the snapshot ${values.snapshot}` : "Daytona's default snapshot"}…`)
  return daytona.create(values.snapshot !== undefined ? { ...base, snapshot: values.snapshot } : base, { timeout: 300 })
}

/** Installs the steery binary into $HOME/.local/bin when the sandbox has none on PATH. */
async function ensureSteery(sandbox: Sandbox): Promise<void> {
  if ((await sh(sandbox, 'command -v steery')).exitCode === 0) return
  console.error(`steery is not in ${sandbox.name}; installing it into $HOME/.local/bin…`)
  const installed = await sh(sandbox, INSTALL_STEERY, 300)
  if (installed.exitCode !== 0) {
    fail(`installing steery in ${sandbox.name} failed (exit ${installed.exitCode}):\n${installed.result}`)
  }
  console.error(`Installed steery. Adding this one line to your image skips this step:\n\n  RUN ${IMAGE_LINE}\n`)
}

/** Stops when the harness is not installed: installing it is the company's, in its snapshot. */
async function requireHarness(sandbox: Sandbox, harness: string): Promise<boolean> {
  const { binary, install } = HARNESSES[harness]!
  if ((await sh(sandbox, `command -v ${binary}`)).exitCode === 0) return true
  console.error(
    `${harness} is not installed in ${sandbox.name} (no \`${binary}\` on PATH).\n` +
      `Install it in your snapshot the way you install your other tools, then run \`up\` again.\n` +
      `Its own installer is:\n\n  ${install}\n`,
  )
  return false
}

/**
 * `steery setup <harness>` in a terminal inside the sandbox, joined to this one: the
 * sign-in code to approve on the phone, the signers to compare and trust, and every
 * question setup asks, are the person's, at their keyboard. Nothing here answers for them.
 */
async function setup(sandbox: Sandbox, name: string, url: string, harness: string): Promise<number> {
  const stdin = process.stdin
  if (!stdin.isTTY || !process.stdout.isTTY) {
    fail('up runs `steery setup` at your terminal, and this is not one: run it in a terminal')
  }
  const words = [
    'steery', 'setup', harness,
    '--url', url,
    '--name', `daytona-${name}`,
    // `steery run` is started below as a toolbox session; a sandbox has no systemd.
    '--no-service',
    '--no-browser',
  ]
  if (values.custody !== undefined) {
    words.push('--custody', values.custody)
  }
  for (const fingerprint of values.trust ?? []) {
    words.push('--trust', fingerprint)
  }
  const command = `${PATH_PREFIX} && exec ${words.map(quote).join(' ')}`

  const pty = await sandbox.process.createPty({
    id: `steery-setup-${Date.now()}`,
    envs: { TERM: process.env.TERM ?? 'xterm-256color', LANG: 'C.UTF-8' },
    cols: process.stdout.columns,
    rows: process.stdout.rows,
    onData: (data) => {
      process.stdout.write(data)
    },
  })
  const resize = () => void pty.resize(process.stdout.columns, process.stdout.rows)
  const input = (chunk: Buffer) => void pty.sendInput(new Uint8Array(chunk))
  try {
    await pty.waitForConnection()
    stdin.setRawMode(true)
    stdin.on('data', input)
    stdin.resume()
    process.stdout.on('resize', resize)
    await pty.sendInput(`${command}\n`)
    const result = await pty.wait()
    if (result.error) console.error(`\n${result.error}`)
    return result.exitCode ?? 1
  } finally {
    process.stdout.off('resize', resize)
    stdin.off('data', input)
    stdin.setRawMode(false)
    stdin.pause()
    await pty.disconnect()
  }
}

/** The command `steery run` runs as, and whether it is still running. */
async function runCommand(sandbox: Sandbox): Promise<{ id: string; exitCode?: number } | undefined> {
  const sessions = await sandbox.process.listSessions()
  const session = sessions.find((s) => s.sessionId === RUN_SESSION)
  return session?.commands.at(-1)
}

async function startRun(sandbox: Sandbox, url: string): Promise<void> {
  const current = await runCommand(sandbox)
  if (current !== undefined && current.exitCode === undefined) {
    console.error('steery run is already running in the sandbox.')
    return
  }
  if (current === undefined) {
    // A session that held a finished run is reused; a sandbox that was stopped has none.
    const sessions = await sandbox.process.listSessions()
    if (!sessions.some((s) => s.sessionId === RUN_SESSION)) await sandbox.process.createSession(RUN_SESSION)
  }
  const started = await sandbox.process.executeSessionCommand(RUN_SESSION, {
    command: `${PATH_PREFIX} && exec steery run --url ${quote(url)}`,
    runAsync: true,
  })
  console.error(`steery run started (session ${RUN_SESSION}, command ${started.cmdId}).`)
}

async function up(): Promise<number> {
  const name = machineName()
  const url = values.url ?? SERVER
  const harness = values.harness ?? 'omp'
  if (HARNESSES[harness] === undefined) {
    fail(`--harness takes ${Object.keys(HARNESSES).join(', ')}, not ${harness}`)
  }
  if (values.custody !== undefined && values.custody !== 'auto' && values.custody !== 'file') {
    fail(`--custody takes auto or file, not ${values.custody}`)
  }
  const daytona = client()

  let sandbox = await find(daytona, name)
  if (sandbox === undefined) {
    sandbox = await create(daytona, name)
  } else {
    console.error(`Found ${sandbox.name} (${sandbox.id}).`)
    await ensureStarted(sandbox)
    // The runtime lives in the sandbox, so the sandbox is the machine and must stay up.
    if (sandbox.autoStopInterval !== 0) await sandbox.setAutostopInterval(0)
  }

  await ensureSteery(sandbox)
  if (!(await requireHarness(sandbox, harness))) return 2

  const code = await setup(sandbox, name, url, harness)
  if (code !== 0) {
    console.error(
      code === 2
        ? '\nsetup stopped at a step that is yours (above). Run `up` again when you are ready.'
        : `\nsetup did not finish (exit ${code}). Run \`up --name ${name}\` again to retry.`,
    )
    return code
  }
  await startRun(sandbox, url)
  console.error(`\nThe machine daytona-${name} is up. Follow its runtime with:\n\n  bun src/up.ts logs --name ${name}\n`)
  return 0
}

async function logs(): Promise<number> {
  const name = machineName()
  const sandbox = (await find(client(), name)) ?? fail(`no sandbox is labelled steery=machine, name=${name}; run \`up --name ${name}\``)
  if (sandbox.state !== SandboxState.STARTED) fail(`${sandbox.name} is ${sandbox.state}; run \`up --name ${name}\``)
  const command = (await runCommand(sandbox)) ?? fail(`steery run was never started in ${sandbox.name}; run \`up --name ${name}\``)
  await sandbox.process.getSessionCommandLogs(
    RUN_SESSION,
    command.id,
    (chunk) => process.stdout.write(chunk),
    (chunk) => process.stderr.write(chunk),
  )
  return 0
}

async function status(): Promise<number> {
  const name = machineName()
  const sandbox = await find(client(), name)
  if (sandbox === undefined) {
    console.log(`${name}: no sandbox`)
    return 1
  }
  console.log(`sandbox   ${sandbox.name} (${sandbox.id})`)
  console.log(`state     ${sandbox.state}${sandbox.errorReason ? `: ${sandbox.errorReason}` : ''}`)
  console.log(`size      ${sandbox.cpu} vCPU, ${sandbox.memory} GiB, ${sandbox.disk} GiB disk`)
  console.log(`autostop  ${sandbox.autoStopInterval === 0 ? 'off' : `${sandbox.autoStopInterval} min`}`)
  console.log(`machine   daytona-${name}`)
  if (sandbox.state !== SandboxState.STARTED) return 0
  const command = await runCommand(sandbox)
  console.log(
    `runtime   ${
      command === undefined ? 'not started' : command.exitCode === undefined ? 'steery run running' : `steery run exited ${command.exitCode}`
    }`,
  )
  const version = await sh(sandbox, 'steery --version')
  console.log(`steery    ${version.exitCode === 0 ? version.result.trim() : 'not installed'}`)
  return 0
}

async function down(): Promise<number> {
  const name = machineName()
  const sandbox = (await find(client(), name)) ?? fail(`no sandbox is labelled steery=machine, name=${name}; run \`up --name ${name}\``)
  if (sandbox.state === SandboxState.STOPPED || sandbox.state === SandboxState.ARCHIVED) {
    console.error(`${sandbox.name} is already ${sandbox.state}.`)
    return 0
  }
  console.error(`Stopping ${sandbox.name}…`)
  await sandbox.stop()
  console.error(`Stopped. Its disk is kept; \`up --name ${name}\` starts it again.`)
  return 0
}

const commands: Record<string, () => Promise<number>> = { up, logs, status, down }

async function main(): Promise<number> {
  const [command, ...rest] = positionals
  if (values.help || command === undefined) {
    console.log(USAGE)
    return values.help ? 0 : 1
  }
  if (rest.length > 0) fail(`unexpected argument: ${rest[0]}`)
  const run = commands[command] ?? fail(`unknown command: ${command}`)
  return run()
}

try {
  process.exit(await main())
} catch (error) {
  if (error instanceof Usage) {
    console.error(`${error.message}\n\n${USAGE}`)
    process.exit(1)
  }
  throw error
}
