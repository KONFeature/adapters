#!/usr/bin/env node
// Steery's adapter for OpenSandbox: it provides sandboxes (PROTOCOL.md §3.2, §5.2, §5.3).
//
// The runtime starts it and speaks the program protocol on standard input and output. It
// talks to OpenSandbox's lifecycle API (specs/sandbox-lifecycle.yml) with fetch, and to the
// execd daemon inside a sandbox (specs/execd-api.yaml, its PTY routes) with fetch and Node's
// WebSocket. The sandbox, its image, credentials and network are the company's and
// OpenSandbox's: a sandbox is given no image, environment, secret or network setting of the
// adapter's own.

import { setTimeout as sleep } from "node:timers/promises";

import { PROTOCOL, ProgramError, method, refusal, serve, type Notify, type Refusal, type wire } from "@steery/sdk";

const RELEASE = "0.1.0";
/** The metadata every sandbox this adapter makes carries, and every list is filtered by. */
const MINE = { steery: "sandbox" } as const;
/** The metadata that finds a sandbox again by the runtime's key. */
const KEY_LABEL = "steery-key";
/** The metadata that keeps the TTL a sandbox was made with, in seconds, for keepalive. */
const TTL_LABEL = "steery-ttl";
/** The TTL keepalive renews by where a sandbox kept none. */
const DEFAULT_TTL_SECONDS = 3600;
/** The port execd listens on inside every sandbox. */
const EXECD_PORT = 44772;
/** OpenSandbox requires an entrypoint with an image; this one only keeps the sandbox up. */
const IDLE_ENTRYPOINT = ["tail", "-f", "/dev/null"];
/** Milliseconds the adapter waits for OpenSandbox, inside the runtime's bounds (§9: 300 s, 60 s). */
const MAKE_MS = 280_000;
const CHANGE_MS = 55_000;
const POLL_MS = 1_000;
/** How long `stream/close` waits for the command to end before it is ended. */
const CLOSE_GRACE_MS = 2_000;
/** The states a sandbox found by key is never reused from. */
const GONE: Record<string, true> = { Stopping: true, Terminated: true };

// execd's PTY WebSocket frames (components/execd/pkg/web/model/pty_ws.go).
const BIN_STDIN = 0x00;
const BIN_STDOUT = 0x01;
const BIN_STDERR = 0x02;
const BIN_REPLAY = 0x03;

let settings: Record<string, string> = {};

/** A sandbox as the lifecycle API answers it. */
interface Box {
  id: string;
  status: { state: string; reason?: string; message?: string };
  metadata?: Record<string, string>;
  createdAt: string;
  expiresAt?: string;
}

interface ListBoxes {
  items: Box[];
  pagination: { hasNextPage: boolean };
}

/** Where execd answers for one sandbox, and the headers it needs. */
interface Execd {
  base: string;
  headers: Record<string, string>;
}

/** An HTTP answer that is no success. */
class HttpError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message);
  }
}

/** The lifecycle API's root: OPENSANDBOX_URL, ending in `/v1`. */
function api(): URL {
  const raw = settings.OPENSANDBOX_URL?.trim();
  if (!raw) throw ProgramError.refused(refusal.REFUSED, "OPENSANDBOX_URL is not set");
  let url: URL;
  try {
    url = new URL(raw);
  } catch {
    throw ProgramError.refused(refusal.REFUSED, `OPENSANDBOX_URL is not a URL: ${raw}`);
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    throw ProgramError.refused(refusal.REFUSED, `OPENSANDBOX_URL is not http or https: ${raw}`);
  }
  const path = url.pathname.replace(/\/+$/, "");
  url.pathname = path.endsWith("/v1") ? path : `${path}/v1`;
  url.search = "";
  url.hash = "";
  return url;
}

/** One line for a person, never holding the API key. */
function line(error: unknown): string {
  let text = error instanceof Error ? error.message : String(error);
  const cause = error instanceof Error ? (error.cause as { message?: unknown } | undefined) : undefined;
  if (cause && typeof cause.message === "string" && cause.message) text = `${text}: ${cause.message}`;
  const key = settings.OPENSANDBOX_API_KEY;
  if (key) text = text.split(key).join("[redacted]");
  text = text.replace(/\s+/g, " ").trim();
  return `OpenSandbox: ${text.length > 300 ? `${text.slice(0, 299)}…` : text}`;
}

/** The refusal an error is; `otherwise` where it is none of not found, auth or network. */
function refuse(error: unknown, otherwise: Refusal): ProgramError {
  if (error instanceof ProgramError) return error;
  let word: Refusal = otherwise;
  if (error instanceof HttpError) {
    if (error.status === 404) word = refusal.NOT_FOUND;
    else if (error.status === 401 || error.status === 403) word = refusal.UNTRUSTED;
  } else if (error instanceof TypeError || (error instanceof DOMException && error.name === "TimeoutError")) {
    // fetch fails with a TypeError when the server cannot be reached at all.
    word = refusal.OFFLINE;
  }
  return ProgramError.refused(word, line(error));
}

/** `fn` with every provider error refused as {@link refuse} says. */
async function provider<T>(otherwise: Refusal, fn: () => Promise<T>): Promise<T> {
  try {
    return await fn();
  } catch (error) {
    throw refuse(error, otherwise);
  }
}

/** `response`'s error as one line: the API's `{code, message}` where it gives one. */
async function failure(response: Response, what: string): Promise<HttpError> {
  let said = "";
  try {
    const body = (await response.text()).trim();
    try {
      const parsed = JSON.parse(body) as Record<string, unknown>;
      const inner = (parsed.detail && typeof parsed.detail === "object" ? parsed.detail : parsed) as Record<string, unknown>;
      said = [inner.code, inner.message].filter((part) => typeof part === "string" && part).join(": ");
      if (!said && typeof parsed.detail === "string") said = parsed.detail;
    } catch {
      said = body;
    }
  } catch {
    // No body to read.
  }
  return new HttpError(response.status, `${what}: HTTP ${response.status}${said ? ` ${said}` : ""}`);
}

/** Call the lifecycle API; its JSON answer, or undefined for an empty one. */
async function lifecycle<T>(verb: string, path: string, what: string, body?: unknown, query?: URLSearchParams): Promise<T> {
  const url = api();
  url.pathname += path;
  if (query) url.search = query.toString();
  const headers: Record<string, string> = { accept: "application/json" };
  const key = settings.OPENSANDBOX_API_KEY;
  if (key) headers["OPEN-SANDBOX-API-KEY"] = key;
  if (body !== undefined) headers["content-type"] = "application/json";
  const response = await fetch(url, {
    method: verb,
    headers,
    ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
    signal: AbortSignal.timeout(MAKE_MS),
  });
  if (!response.ok) throw await failure(response, what);
  const text = await response.text();
  return (text ? JSON.parse(text) : undefined) as T;
}

function get(id: string): Promise<Box> {
  return lifecycle<Box>("GET", `/sandboxes/${encodeURIComponent(id)}`, "get sandbox");
}

/** Read the sandbox until its state is one of `want`, or until `ms` have passed. */
async function until(id: string, want: readonly string[], ms: number): Promise<Box> {
  const deadline = Date.now() + ms;
  for (;;) {
    const box = await get(id);
    if (want.includes(box.status.state)) return box;
    if (box.status.state === "Failed" || box.status.state === "Terminated") {
      throw ProgramError.refused(refusal.START_FAILED, line(`sandbox ${id} is ${box.status.state}${box.status.message ? `: ${box.status.message}` : ""}`));
    }
    if (Date.now() >= deadline) {
      throw ProgramError.refused(refusal.START_FAILED, line(`sandbox ${id} is still ${box.status.state}, not ${want.join(" or ")}`));
    }
    await sleep(POLL_MS);
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

/** Every sandbox carrying all of `metadata`, page by page. */
async function listed(metadata: Record<string, string>): Promise<Box[]> {
  const boxes: Box[] = [];
  for (let page = 1; ; page++) {
    const query = new URLSearchParams({
      metadata: new URLSearchParams(metadata).toString(),
      page: String(page),
      pageSize: "200",
    });
    const answer = await lifecycle<ListBoxes>("GET", "/sandboxes", "list sandboxes", undefined, query);
    boxes.push(...answer.items);
    if (!answer.pagination.hasNextPage || answer.items.length === 0) return boxes;
  }
}

async function list(params: wire.SandboxList | null): Promise<wire.SandboxListed> {
  const labels = { ...labelsOf(params?.labels, "labels"), ...MINE };
  return provider(refusal.REFUSED, async () => {
    const sandboxes = (await listed(labels)).map((box): wire.Sandbox => {
      const created = Date.parse(box.createdAt);
      return {
        sandbox: box.id,
        state: box.status.state,
        labels: box.metadata ?? {},
        created_at: Number.isNaN(created) ? 0 : created,
      };
    });
    return { sandboxes };
  });
}

async function ensure(params: wire.Ensure): Promise<wire.Named> {
  const key = text(params, "key");
  const metadata = { ...labelsOf(params.labels, "labels"), ...MINE, [KEY_LABEL]: key };
  const source = params.source ?? undefined;
  const found = await provider(refusal.REFUSED, async () =>
    (await listed({ ...MINE, [KEY_LABEL]: key })).find((box) => !GONE[box.status.state]),
  );
  if (found) return { sandbox: found.id };
  if (!source) {
    throw ProgramError.refused(
      refusal.NO_PLACE,
      "OpenSandbox needs an image or a snapshot to make a sandbox: it has no default. Name one as the machine's source.",
    );
  }
  // `resourceLimits` is required outside pool mode; empty, the runtime's own defaults apply.
  const body =
    "image" in source
      ? { image: { uri: source.image }, entrypoint: IDLE_ENTRYPOINT, resourceLimits: {}, metadata }
      : { snapshotId: source.snapshot, resourceLimits: {}, metadata };
  const made = await provider(refusal.START_FAILED, () => lifecycle<Box>("POST", "/sandboxes", "create sandbox", body));
  if (made.expiresAt) {
    // The provider gave it a TTL: keep it, so keepalive renews by the same.
    const ttl = Math.round((Date.parse(made.expiresAt) - Date.parse(made.createdAt)) / 1000);
    if (ttl > 0) {
      await lifecycle("PATCH", `/sandboxes/${encodeURIComponent(made.id)}/metadata`, "keep sandbox TTL", {
        [TTL_LABEL]: String(ttl),
      }).catch((error: unknown) => process.stderr.write(`${line(error)}\n`));
    }
  }
  return { sandbox: made.id };
}

async function start(params: wire.Named): Promise<wire.State> {
  const id = text(params, "sandbox");
  return provider(refusal.START_FAILED, async () => {
    let box = await get(id);
    if (box.status.state === "Pausing") box = await until(id, ["Paused"], MAKE_MS);
    if (box.status.state === "Paused") {
      await lifecycle("POST", `/sandboxes/${encodeURIComponent(id)}/resume`, "resume sandbox");
      box = await until(id, ["Running"], MAKE_MS);
    } else if (box.status.state === "Pending" || box.status.state === "Resuming") {
      box = await until(id, ["Running"], MAKE_MS);
    }
    return { state: box.status.state };
  });
}

/** Pause: OpenSandbox's only stop that keeps the sandbox. */
async function pause(params: wire.Named): Promise<wire.State> {
  const id = text(params, "sandbox");
  return provider(refusal.REFUSED, async () => {
    let box = await get(id);
    if (box.status.state === "Pending" || box.status.state === "Resuming") box = await until(id, ["Running"], CHANGE_MS);
    if (box.status.state === "Running") {
      await lifecycle("POST", `/sandboxes/${encodeURIComponent(id)}/pause`, "pause sandbox");
      box = await until(id, ["Paused"], CHANGE_MS);
    } else if (box.status.state === "Pausing") {
      box = await until(id, ["Paused"], CHANGE_MS);
    }
    return { state: box.status.state };
  });
}

async function remove(params: wire.Named): Promise<wire.State> {
  const id = text(params, "sandbox");
  return provider(refusal.REFUSED, async () => {
    await lifecycle("DELETE", `/sandboxes/${encodeURIComponent(id)}`, "delete sandbox");
    try {
      return { state: (await get(id)).status.state };
    } catch (error) {
      if (error instanceof HttpError && error.status === 404) return { state: "Terminated" };
      throw error;
    }
  });
}

/** Renew the sandbox's expiration by the TTL it was made with; one with none never expires. */
async function keepalive(params: wire.Named): Promise<Record<string, never>> {
  const id = text(params, "sandbox");
  return provider(refusal.REFUSED, async () => {
    const box = await get(id);
    if (!box.expiresAt) return {};
    const kept = Number(box.metadata?.[TTL_LABEL]);
    const ttl = Number.isFinite(kept) && kept > 0 ? kept : DEFAULT_TTL_SECONDS;
    const expiresAt = new Date(Date.now() + ttl * 1000).toISOString();
    await lifecycle("POST", `/sandboxes/${encodeURIComponent(id)}/renew-expiration`, "renew sandbox", { expiresAt });
    return {};
  });
}

/** Where the sandbox's execd answers: its endpoint for port 44772, with the headers it names. */
async function execd(id: string): Promise<Execd> {
  const endpoint = await lifecycle<{ endpoint: string; headers?: Record<string, string> }>(
    "GET",
    `/sandboxes/${encodeURIComponent(id)}/endpoints/${EXECD_PORT}`,
    "get execd endpoint",
  );
  const scheme = api().protocol;
  const base = /^https?:\/\//.test(endpoint.endpoint) ? endpoint.endpoint : `${scheme}//${endpoint.endpoint}`;
  return { base: base.replace(/\/+$/, ""), headers: endpoint.headers ?? {} };
}

async function execdCall(target: Execd, verb: string, path: string, what: string, body?: FormData, json?: unknown): Promise<Response> {
  const headers: Record<string, string> = { ...target.headers };
  if (json !== undefined) headers["content-type"] = "application/json";
  const response = await fetch(`${target.base}${path}`, {
    method: verb,
    headers,
    ...(json !== undefined ? { body: JSON.stringify(json) } : body !== undefined ? { body } : {}),
    signal: AbortSignal.timeout(CHANGE_MS),
  });
  if (!response.ok) throw await failure(response, what);
  return response;
}

async function put(params: wire.Put): Promise<Record<string, never>> {
  const id = text(params, "sandbox");
  const path = text(params, "path");
  if (!path.startsWith("/")) throw ProgramError.invalid("path must be absolute");
  if (typeof params.content !== "string") throw ProgramError.invalid("content must be base64");
  if (!Number.isInteger(params.mode) || params.mode < 0) throw ProgramError.invalid("mode must be a Unix mode");
  const content = Buffer.from(params.content, "base64");
  return provider(refusal.REFUSED, async () => {
    const target = await execd(id);
    // execd reads `mode` as octal digits written in decimal: 0o644 is 644.
    const metadata = { path, mode: Number((params.mode & 0o7777).toString(8)) };
    const form = new FormData();
    form.append("metadata", new Blob([JSON.stringify(metadata)], { type: "application/json" }), "metadata.json");
    form.append("file", new Blob([content], { type: "application/octet-stream" }), path.split("/").pop() || "file");
    await execdCall(target, "POST", "/files/upload", "upload file", form);
    return {};
  });
}

/** One argument as a POSIX shell reads it back. */
function quote(arg: string): string {
  return /^[A-Za-z0-9_\/.:=@%+,-]+$/.test(arg) ? arg : `'${arg.replace(/'/g, `'\\''`)}'`;
}

/** One open channel: an execd pipe-mode PTY session and the WebSocket attached to it. */
interface Channel {
  socket: WebSocket;
  target: Execd;
  session: string;
  exit?: number;
  closed: Promise<void>;
}

const channels = new Map<string, Channel>();

/**
 * A stream channel: a pipe-mode execd PTY session running `command`, joined over its
 * WebSocket. stdout goes to the runtime as `stream/data`; stderr goes to this adapter's log.
 */
async function channel(notify: Notify, params: wire.SandboxChannel): Promise<wire.Reach> {
  const id = text(params, "sandbox");
  if (!Array.isArray(params.command) || params.command.length === 0 || !params.command.every((arg) => typeof arg === "string")) {
    throw ProgramError.invalid("command must be a non-empty list of strings");
  }
  const command = params.command.map(quote).join(" ");
  return provider(refusal.REFUSED, async () => {
    const target = await execd(id);
    const made = (await (await execdCall(target, "POST", "/pty", "open pty session", undefined, { command })).json()) as {
      session_id?: string;
    };
    const session = made.session_id;
    if (!session) throw new Error("execd answered no session id");
    const url = new URL(`${target.base}/pty/${encodeURIComponent(session)}/ws`);
    url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
    url.searchParams.set("pty", "0");
    const socket = new WebSocket(url, { headers: target.headers });
    socket.binaryType = "arraybuffer";

    const connected = Promise.withResolvers<void>();
    const ended = Promise.withResolvers<void>();
    const open: Channel = { socket, target, session, closed: ended.promise };
    let announced = false;

    socket.addEventListener("message", (event: MessageEvent) => {
      if (typeof event.data === "string") {
        let frame: { type?: string; exit_code?: number; error?: string; code?: string };
        try {
          frame = JSON.parse(event.data) as typeof frame;
        } catch {
          return;
        }
        if (frame.type === "connected") connected.resolve();
        else if (frame.type === "exit" && typeof frame.exit_code === "number") open.exit = frame.exit_code;
        else if (frame.type === "error") {
          const said = `execd: ${frame.code ?? "error"}${frame.error ? `: ${frame.error}` : ""}`;
          process.stderr.write(`[${session}] ${said}\n`);
          connected.reject(new Error(said));
        }
        return;
      }
      const bytes = new Uint8Array(event.data as ArrayBuffer);
      if (bytes.length === 0) return;
      const kind = bytes[0];
      // A replay frame holds output written before this socket joined, after an 8-byte offset.
      const payload = kind === BIN_REPLAY ? bytes.subarray(9) : bytes.subarray(1);
      if (payload.length === 0) return;
      if (kind === BIN_STDOUT || kind === BIN_REPLAY) {
        notify(method.STREAM_DATA, { stream: session, bytes: Buffer.from(payload).toString("base64") } satisfies wire.StreamData);
      } else if (kind === BIN_STDERR) {
        process.stderr.write(`[${session}] `);
        process.stderr.write(payload);
      }
    });
    socket.addEventListener("close", (event) => {
      connected.reject(new Error(`execd closed the channel before it was joined (${event.code}${event.reason ? ` ${event.reason}` : ""})`));
      if (channels.get(session) === open) channels.delete(session);
      if (announced) {
        const closed: wire.StreamClosed = { stream: session };
        if (open.exit !== undefined) closed.exit = open.exit;
        notify(method.STREAM_CLOSED, closed);
      }
      ended.resolve();
    });
    // The close event follows an error event; the close says what ended.
    socket.addEventListener("error", () => {});

    try {
      await connected.promise;
    } catch (error) {
      socket.close();
      await execdCall(target, "DELETE", `/pty/${encodeURIComponent(session)}`, "delete pty session").catch(() => undefined);
      throw error;
    }
    channels.set(session, open);
    announced = true;
    return { stream: session };
  });
}

function opened(params: unknown): Channel {
  const stream = text(params, "stream");
  const open = channels.get(stream);
  if (!open) throw ProgramError.refused(refusal.NOT_FOUND, `OpenSandbox: no open channel ${stream}`);
  return open;
}

function write(params: wire.StreamWrite): Record<string, never> {
  const open = opened(params);
  if (typeof params.bytes !== "string") throw ProgramError.invalid("bytes must be base64");
  const bytes = Buffer.from(params.bytes, "base64");
  if (open.socket.readyState !== WebSocket.OPEN) throw ProgramError.refused(refusal.OFFLINE, "OpenSandbox: the channel is closing");
  const frame = new Uint8Array(1 + bytes.length);
  frame[0] = BIN_STDIN;
  frame.set(bytes, 1);
  open.socket.send(frame);
  return {};
}

/**
 * End a channel. execd's frames have no way to close only standard input, so the command is
 * sent SIGHUP in a JSON signal frame, as a terminal's closing would; whatever still runs after
 * a grace is ended with the session (DELETE /pty/{id}), and then the socket is closed.
 */
async function close(params: wire.StreamClose): Promise<Record<string, never>> {
  const open = opened(params);
  const grace = (ms: number) => Promise.race([open.closed, sleep(ms)]);
  if (open.socket.readyState === WebSocket.OPEN) {
    open.socket.send(JSON.stringify({ type: "signal", signal: "SIGHUP" }));
    await grace(CLOSE_GRACE_MS);
  }
  await execdCall(open.target, "DELETE", `/pty/${encodeURIComponent(open.session)}`, "delete pty session").catch(
    (error: unknown) => {
      if (!(error instanceof HttpError && error.status === 404)) process.stderr.write(`${line(error)}\n`);
    },
  );
  await grace(CLOSE_GRACE_MS);
  if (open.socket.readyState !== WebSocket.CLOSED) open.socket.close(1000, "closed by steery");
  await grace(CLOSE_GRACE_MS);
  return {};
}

await serve((notify) => ({
  initialize(params): wire.Initialized {
    settings = { ...(params.settings ?? {}) };
    return {
      protocol: PROTOCOL,
      program: { id: "opensandbox", kind: "machine", release: RELEASE, label: "OpenSandbox" },
      machine: {
        provides: "sandboxes",
        keeps: "filesystem",
        idle: "ttl",
        optional: ["pause", "keepalive"],
        channel: "stream",
        os: "linux",
      },
      settings: [
        {
          name: "OPENSANDBOX_URL",
          secret: false,
          required: true,
          about: "Your OpenSandbox server's URL, as its SDKs take it: http://localhost:8080",
        },
        {
          name: "OPENSANDBOX_API_KEY",
          secret: true,
          required: false,
          about: "The server's API key (its server.api_key), where it has one",
        },
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
      case method.SANDBOX_PAUSE:
        return pause(params as wire.Named);
      case method.SANDBOX_DELETE:
        return remove(params as wire.Named);
      case method.SANDBOX_KEEPALIVE:
        return keepalive(params as wire.Named);
      case method.SANDBOX_PUT:
        return put(params as wire.Put);
      case method.SANDBOX_CHANNEL:
        return channel(notify, params as wire.SandboxChannel);
      case method.STREAM_WRITE:
        return write(params as wire.StreamWrite);
      case method.STREAM_CLOSE:
        return close(params as wire.StreamClose);
      default:
        throw ProgramError.unknown();
    }
  },
}));

// Open channels hold the event loop; the runtime has said shutdown.
for (const open of channels.values()) open.socket.close(1001, "steery adapter shut down");
process.exit(0);
