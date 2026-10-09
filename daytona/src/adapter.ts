#!/usr/bin/env node
// Steery's adapter for Daytona: it provides sandboxes (PROTOCOL.md §3.2, §5.2).
//
// The runtime starts it and speaks the program protocol on standard input and output. The
// Daytona client is built from the settings `initialize` passes, and from nothing else: the
// sandbox, its image, credentials and network are the company's and Daytona's, so no
// sandbox is given an image, environment, secret or network setting of the adapter's own.

import "./isolate.js";
import { Buffer } from "node:buffer";
import { setTimeout as sleep } from "node:timers/promises";
import * as daytona from "@daytona/sdk";
import type { Daytona as DaytonaClient, Sandbox } from "@daytona/sdk";
import { PROTOCOL, ProgramError, method, refusal, serve, type Refusal, type wire } from "@steery/sdk";

const RELEASE = "0.1.0";
/** The label every sandbox this adapter makes carries, and every list is filtered by. */
const MINE = { steery: "sandbox" } as const;
/** The label that finds a sandbox again by the runtime's key. */
const KEY_LABEL = "steery-key";
/** The host Daytona's SSH access docs name, where its answer names none. */
const SSH_HOST = "ssh.app.daytona.io";
/** How long a channel's SSH access token lives, in minutes. */
const SSH_MINUTES = 60;
/** Seconds the adapter waits for Daytona, inside the runtime's bounds (§9: 300 s, 60 s). */
const LONG_SECONDS = 280;
const CHANGE_SECONDS = 55;
/** Seconds for one shell command in the sandbox: a size check, a chmod, a removal. */
const COMMAND_SECONDS = 5;

let settings: Record<string, string> = {};
let client: DaytonaClient | undefined;

/** The Daytona client, built once from the settings `initialize` gave. */
function connect(): DaytonaClient {
  if (client) return client;
  const apiKey = settings.DAYTONA_API_KEY;
  if (!apiKey) throw ProgramError.refused(refusal.UNTRUSTED, "DAYTONA_API_KEY is not set");
  try {
    client = new daytona.Daytona({
      apiKey,
      apiUrl: settings.DAYTONA_API_URL || "https://app.daytona.io/api",
      ...(settings.DAYTONA_TARGET ? { target: settings.DAYTONA_TARGET } : {}),
      otelEnabled: false,
    });
  } catch (error) {
    throw refuse(error, refusal.REFUSED);
  }
  return client;
}

/** One line for a person, from a provider error, never holding the API key. */
function line(error: unknown): string {
  let text = error instanceof Error ? error.message : String(error);
  const key = settings.DAYTONA_API_KEY;
  if (key) text = text.split(key).join("[redacted]");
  text = text.replace(/\s+/g, " ").trim();
  return `Daytona: ${text.length > 300 ? `${text.slice(0, 299)}…` : text}`;
}

const NETWORK_CODES: Record<string, true> = {
  ECONNREFUSED: true, ECONNRESET: true, ENOTFOUND: true, EAI_AGAIN: true, ETIMEDOUT: true,
  ENETUNREACH: true, EHOSTUNREACH: true, ECONNABORTED: true, EPIPE: true,
};

/** The refusal a provider error is; `otherwise` where it is none of not found, auth or network. */
function refuse(error: unknown, otherwise: Refusal): ProgramError {
  if (error instanceof ProgramError) return error;
  // Before the connection test: Daytona's transport timeout is a connection error too, but a
  // call that ran out of time reached Daytona and is not offline.
  if (error instanceof daytona.DaytonaTimeoutError) return ProgramError.refused(refusal.REFUSED, `${line(error)} (timed out)`);
  let word: Refusal = otherwise;
  if (error instanceof daytona.DaytonaNotFoundError) word = refusal.NOT_FOUND;
  else if (error instanceof daytona.DaytonaAuthenticationError || error instanceof daytona.DaytonaForbiddenError)
    word = refusal.UNTRUSTED;
  else if (error instanceof daytona.DaytonaConnectionError) word = refusal.OFFLINE;
  else if (error instanceof daytona.DaytonaError) {
    if (error.statusCode === 404) word = refusal.NOT_FOUND;
    else if (error.statusCode === 401 || error.statusCode === 403) word = refusal.UNTRUSTED;
  } else {
    const code = (error as { code?: unknown } | null)?.code;
    if (typeof code === "string" && NETWORK_CODES[code]) word = refusal.OFFLINE;
  }
  return ProgramError.refused(word, line(error));
}

/** `fn` with every provider error refused as {@link refuse} says. */
async function provider<T>(otherwise: Refusal, fn: (client: DaytonaClient) => Promise<T>): Promise<T> {
  const daytonaClient = connect();
  try {
    return await fn(daytonaClient);
  } catch (error) {
    throw refuse(error, otherwise);
  }
}

function text(params: unknown, name: string): string {
  const value = (params as Record<string, unknown> | null)?.[name];
  if (typeof value !== "string" || value === "") throw ProgramError.invalid(`${name} must be a non-empty string`);
  return value;
}

function labelsOf(value: unknown, name: string): Record<string, string> {
  if (value === undefined || value === null) return {};
  if (typeof value !== "object" || Array.isArray(value)) throw ProgramError.invalid(`${name} must be an object of strings`);
  for (const inner of Object.values(value)) {
    if (typeof inner !== "string") throw ProgramError.invalid(`${name} must be an object of strings`);
  }
  return value as Record<string, string>;
}

function stateOf(sandbox: Sandbox, otherwise: string): wire.State {
  return { state: sandbox.state ?? otherwise };
}

/** One argument as a POSIX shell reads it back. */
function quote(arg: string): string {
  return /^[A-Za-z0-9_\/.:=@%+,-]+$/.test(arg) ? arg : `'${arg.replace(/'/g, `'\\''`)}'`;
}

async function list(params: wire.SandboxList | null): Promise<wire.SandboxListed> {
  const labels = { ...labelsOf(params?.labels, "labels"), ...MINE };
  return provider(refusal.REFUSED, async (c) => {
    const sandboxes: wire.Sandbox[] = [];
    for await (const sandbox of c.list({ labels })) {
      const created = sandbox.createdAt ? Date.parse(sandbox.createdAt) : NaN;
      sandboxes.push({
        sandbox: sandbox.id,
        state: sandbox.state ?? "unknown",
        labels: sandbox.labels ?? {},
        created_at: Number.isNaN(created) ? 0 : created,
      });
    }
    return { sandboxes };
  });
}

async function ensure(params: wire.Ensure): Promise<wire.Named> {
  const key = text(params, "key");
  const labels = { ...labelsOf(params.labels, "labels"), ...MINE, [KEY_LABEL]: key };
  const source = params.source ?? undefined;
  const found = await provider(refusal.REFUSED, async (c) => {
    for await (const sandbox of c.list({ labels: { ...MINE, [KEY_LABEL]: key }, limit: 1 })) return sandbox;
    return undefined;
  });
  if (found) return { sandbox: found.id };
  return provider(refusal.START_FAILED, async (c) => {
    const made =
      source && "image" in source
        ? await c.create({ image: source.image, labels }, { timeout: LONG_SECONDS })
        : await c.create(source ? { snapshot: source.snapshot, labels } : { labels }, { timeout: LONG_SECONDS });
    return { sandbox: made.id };
  });
}

async function start(params: wire.Named): Promise<wire.State> {
  const id = text(params, "sandbox");
  return provider(refusal.START_FAILED, async (c) => {
    const sandbox = await c.get(id);
    await sandbox.start(LONG_SECONDS);
    return stateOf(sandbox, "started");
  });
}

async function stop(params: wire.Named): Promise<wire.State> {
  const id = text(params, "sandbox");
  return provider(refusal.REFUSED, async (c) => {
    const sandbox = await c.get(id);
    await sandbox.stop(CHANGE_SECONDS);
    return stateOf(sandbox, "stopped");
  });
}

async function remove(params: wire.Named): Promise<wire.State> {
  const id = text(params, "sandbox");
  return provider(refusal.REFUSED, async (c) => {
    const sandbox = await c.get(id);
    await sandbox.delete(CHANGE_SECONDS, true);
    return stateOf(sandbox, "destroyed");
  });
}

/** Archive: Daytona archives only a stopped sandbox, so a started one is stopped first. */
async function archive(params: wire.Named): Promise<wire.State> {
  const id = text(params, "sandbox");
  return provider(refusal.REFUSED, async (c) => {
    const deadline = Date.now() + LONG_SECONDS * 1000;
    const sandbox = await c.get(id);
    const was = sandbox.state;
    if (was !== daytona.SandboxState.ARCHIVED) {
      if (was !== daytona.SandboxState.STOPPED) await sandbox.stop(CHANGE_SECONDS / 2);
      await sandbox.archive();
    }
    for (;;) {
      // Read again each pass: refreshData changes it behind the compiler's narrowing.
      const state: string | undefined = sandbox.state;
      if (state === daytona.SandboxState.ARCHIVED) break;
      if (state === daytona.SandboxState.ERROR) throw new Error(`sandbox ${id} failed to archive: ${sandbox.errorReason ?? "error"}`);
      if (Date.now() > deadline) throw new Error(`sandbox ${id} timed out after ${LONG_SECONDS} s still ${state ?? "unknown"}, not archived`);
      await sleep(2000);
      await sandbox.refreshData();
    }
    return stateOf(sandbox, "archived");
  });
}

async function keepalive(params: wire.Named): Promise<Record<string, never>> {
  const id = text(params, "sandbox");
  return provider(refusal.REFUSED, async (c) => {
    await (await c.get(id)).refreshActivity();
    return {};
  });
}

async function put(params: wire.Put): Promise<Record<string, never>> {
  const id = text(params, "sandbox");
  const path = text(params, "path");
  if (typeof params.content !== "string") throw ProgramError.invalid("content must be base64");
  if (!Number.isInteger(params.mode) || params.mode < 0 || params.mode > 0o7777) throw ProgramError.invalid("mode must be Unix mode bits");
  const content = Buffer.from(params.content, "base64");
  return provider(refusal.REFUSED, async (c) => {
    const sandbox = await c.get(id);
    const run = async (command: string) => sandbox.process.executeCommand(command, undefined, undefined, COMMAND_SECONDS);
    let timedOut = false;
    try {
      await sandbox.fs.uploadFile(content, path, LONG_SECONDS);
    } catch (error) {
      if (!(error instanceof daytona.DaytonaTimeoutError)) throw error;
      timedOut = true;
    }
    // An upload can answer before the file is whole, or time out half-written: the size on
    // the sandbox is the proof, and a partial file is removed rather than left to be run.
    const stat = await run(`stat -c %s ${quote(path)}`);
    const written = stat.exitCode === 0 ? Number(stat.result.trim()) : 0;
    if (timedOut || written !== content.length) {
      await run(`rm -f ${quote(path)}`);
      const why = timedOut ? `the upload timed out after ${LONG_SECONDS} s` : "the upload ended short";
      throw ProgramError.refused(
        refusal.REFUSED,
        `Daytona: ${why}: ${path} had ${written} of ${content.length} bytes written; removed it (fetch a large file from inside the sandbox)`,
      );
    }
    const chmod = await run(`chmod ${params.mode.toString(8)} ${quote(path)}`);
    if (chmod.exitCode !== 0) throw new Error(`chmod ${path} exited ${chmod.exitCode}: ${chmod.result}`);
    return {};
  });
}

/** An argv channel: ssh to Daytona's gateway with a 60-minute access token, running `command`. */
async function channel(params: wire.SandboxChannel): Promise<wire.Reach> {
  const id = text(params, "sandbox");
  const command = params.command;
  if (!Array.isArray(command) || command.length === 0 || command.some((arg) => typeof arg !== "string"))
    throw ProgramError.invalid("command must be a non-empty array of strings");
  return provider(refusal.REFUSED, async (c) => {
    const access = await (await c.get(id)).createSshAccess(SSH_MINUTES);
    // Daytona answers the command it means (`ssh <token>@<host>` with any `-p <port>`); its
    // host and port are taken from there, and the docs' host where it names none.
    const words = (access.sshCommand ?? "").trim().split(/\s+/);
    const target = words.find((word) => word.includes("@"));
    const host = target ? target.slice(target.lastIndexOf("@") + 1) : SSH_HOST;
    const portAt = words.indexOf("-p");
    const port = portAt >= 0 ? words[portAt + 1] : undefined;
    const argv = ["ssh", "-T", "-o", "StrictHostKeyChecking=accept-new"];
    if (port) argv.push("-p", port);
    argv.push(`${access.token}@${host || SSH_HOST}`, command.map(quote).join(" "));
    return { argv, env: {} };
  });
}

await serve({
  initialize(params): wire.Initialized {
    settings = { ...(params.settings ?? {}) };
    client = undefined;
    const facts: wire.SandboxFacts = {
      keeps: "filesystem",
      persist: null,
      idle: "activity",
      optional: ["archive", "keepalive"],
      channel: "argv",
      os: "linux",
    };
    return {
      protocol: PROTOCOL,
      program: { id: "daytona", kind: "machine", release: RELEASE, label: "Daytona" },
      provides: "sandboxes",
      facts,
      settings: [
        { name: "DAYTONA_API_KEY", secret: true, required: true, about: "An API key from app.daytona.io/dashboard/keys" },
        { name: "DAYTONA_API_URL", secret: false, required: false, about: "Your Daytona API URL, for a self-hosted Daytona; absent: https://app.daytona.io/api" },
        { name: "DAYTONA_TARGET", secret: false, required: false, about: "The Daytona region sandboxes are made in (us, eu); absent: your organization's default" },
      ],
    };
  },
  call(name, params) {
    switch (name) {
      case method.SANDBOXES_LIST:
        return list(params as wire.SandboxList | null);
      case method.SANDBOX_ENSURE:
        return ensure(params as wire.Ensure);
      case method.SANDBOX_START:
        return start(params as wire.Named);
      case method.SANDBOX_STOP:
        return stop(params as wire.Named);
      case method.SANDBOX_DELETE:
        return remove(params as wire.Named);
      case method.SANDBOX_ARCHIVE:
        return archive(params as wire.Named);
      case method.SANDBOX_KEEPALIVE:
        return keepalive(params as wire.Named);
      case method.SANDBOX_PUT:
        return put(params as wire.Put);
      case method.SANDBOX_CHANNEL:
        return channel(params as wire.SandboxChannel);
      default:
        throw ProgramError.unknown();
    }
  },
});

// Daytona's event stream holds the event loop open; the runtime has said shutdown.
process.exit(0);
