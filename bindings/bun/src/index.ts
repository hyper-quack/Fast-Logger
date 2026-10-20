// rotlog for Bun: a thin, allocation-light front end over the Rust core.
//
// Hot path per call: one Uint8Array read for the level check, one
// JSON.stringify of the fields, two Buffer.write calls into a reused buffer,
// and one FFI call. Timestamping, ordering, buffering, rotation, gzip and
// uploads all happen in Rust on background threads.

import { AsyncLocalStorage } from "node:async_hooks";
import { resolve } from "node:path";
import { toArrayBuffer, type Pointer } from "bun:ffi";
import { lastError, native, nativePath, type Native } from "./native";

export type Level = "trace" | "debug" | "info" | "warn" | "error" | "fatal" | "off";
export type Fields = Record<string, unknown>;
type Duration = string | number;
type Size = string | number;

const LEVELS: Level[] = ["trace", "debug", "info", "warn", "error", "fatal", "off"];
const L = { trace: 0, debug: 1, info: 2, warn: 3, error: 4, fatal: 5, off: 6 } as const;

export interface UploadOptions {
  /** Google Cloud Storage bucket. Upload is enabled when set. */
  bucket: string;
  /** Prepended to object names, e.g. "prod/". */
  prefix?: string;
  /** Object name template: {prefix} {host} {name} {file} {yyyy} {mm} {dd} {hh}. */
  object?: string;
  /** Delay after rotation/compression before uploading. */
  after?: Duration;
  /** Delete the local archive once uploaded. */
  deleteLocal?: boolean;
  /** Service-account or authorized-user JSON (default: GOOGLE_APPLICATION_CREDENTIALS, gcloud ADC, metadata server). */
  credentials?: string;
  /** Static OAuth token (tests, short jobs). */
  accessToken?: string;
  /** API endpoint (emulators). */
  endpoint?: string;
  timeout?: Duration;
}

export interface LoggerOptions {
  /** Log directory (default "logs"). */
  dir?: string;
  /** Stream name; active file is <dir>/<name>.<ext> (default "app"). */
  name?: string;
  ext?: string;
  level?: Level;
  format?: "text" | "json";
  timezone?: "local" | "utc";
  /** Echo lines to the console. `true` = stderr. */
  console?: boolean | "stdout" | "stderr" | "off";
  /** Max time a record waits in memory (default "100ms"; 0 = immediate). */
  flushInterval?: Duration;
  bufferSize?: Size;
  maxBuffer?: Size;
  overflow?: "block" | "drop";
  fsync?: "never" | "rotate" | "always";
  /** Write error/fatal immediately (default true). */
  flushOnError?: boolean;
  /** Background compress/upload/retention period (default "1m"; 0 = off). */
  maintenanceInterval?: Duration;
  rotation?: {
    /** never | minutely | hourly | daily | weekly[:sun] | 15m | 6h | 2d ... (default daily) */
    every?: string;
    maxSize?: Size;
    /** Archives kept locally (default 30, 0 = all). */
    keep?: number;
    maxAge?: Duration;
  };
  /** Gzip archives (default on, right after rotation). */
  compress?: boolean | { enabled?: boolean; after?: Duration; level?: number };
  upload?: UploadOptions;
  /** Explicit path to librotlog.{dylib,so}. */
  libPath?: string;
  /** Merge the active request context (see withRequestLogging) into every record (default true). */
  useContext?: boolean;
  /**
   * Flush on SIGINT/SIGTERM (exiting if no other handler is installed) and
   * rotate on SIGHUP (default true).
   */
  handleSignals?: boolean;
}

export interface RequestInfo {
  method: string;
  path: string;
  status: number;
  durationMs?: number;
  ip?: string;
  user?: string;
  reqId?: string;
  [extra: string]: unknown;
}

export interface Stats {
  pid: number;
  name: string;
  active: string;
  level: Level;
  uptime_secs: number;
  records: number;
  bytes: number;
  dropped: number;
  pending_bytes: number;
  file_bytes: number;
  rotations: number;
  write_errors: number;
  compressed: number;
  uploaded: number;
  upload_errors: number;
  deleted: number;
  maintenance_completed: number;
  last_error: string | null;
  /** The writer's rotation / compression / upload settings, summarized. */
  policy: string;
}

// ---- request context ---------------------------------------------------

/** Fields attached to every record logged while handling one request. */
export class RequestContext {
  private fields: Fields;
  private cached: string | undefined;

  constructor(fields: Fields = {}) {
    this.fields = { ...fields };
  }

  set(key: string, value: unknown): this {
    this.fields[key] = value;
    this.cached = undefined;
    return this;
  }

  get(key: string): unknown {
    return this.fields[key];
  }

  /** JSON object body without braces (cached until the next `set`). */
  inner(): string {
    if (this.cached === undefined) this.cached = innerJson(this.fields);
    return this.cached;
  }
}

const als = new AsyncLocalStorage<RequestContext>();

/** The context of the request being handled, if any. */
export function currentContext(): RequestContext | undefined {
  return als.getStore();
}

/** Runs `fn` with `fields` attached to every record it logs (including async continuations). */
export function withContext<T>(fields: Fields | RequestContext, fn: () => T): T {
  const ctx = fields instanceof RequestContext ? fields : new RequestContext(fields);
  return als.run(ctx, fn);
}

/** Records the authenticated user for the current request (shows up in its request log line). */
export function setUser(user: string | number | undefined): void {
  if (user !== undefined) als.getStore()?.set("user", String(user));
}

// ---- serialization -------------------------------------------------------

export function serializeError(e: unknown, depth = 0): Fields {
  if (!(e instanceof Error)) return { message: String(e) };
  const out: Fields = { name: e.name, message: e.message };
  const code = (e as { code?: unknown }).code;
  if (code !== undefined) out.code = code;
  if (e.stack) out.stack = e.stack;
  if (e.cause !== undefined && depth < 3) out.cause = serializeError(e.cause, depth + 1);
  return out;
}

function safeReplacer() {
  const seen = new WeakSet<object>();
  return (_k: string, v: unknown) => {
    if (v instanceof Error) return serializeError(v);
    if (typeof v === "bigint") return v.toString();
    if (typeof v === "object" && v !== null) {
      if (seen.has(v)) return "[circular]";
      seen.add(v);
    }
    return v;
  };
}

function stringify(o: Fields): string {
  for (const k in o) {
    if (o[k] instanceof Error) return JSON.stringify(o, safeReplacer());
  }
  try {
    return JSON.stringify(o);
  } catch {
    try {
      return JSON.stringify(o, safeReplacer());
    } catch {
      return '{"fields":"[unserializable]"}';
    }
  }
}

function innerJson(o: Fields): string {
  const s = stringify(o);
  return s.length > 2 ? s.slice(1, -1) : "";
}

function snake(o: Record<string, unknown>): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(o)) {
    if (v === undefined) continue;
    const key = k.replace(/[A-Z]/g, (c) => "_" + c.toLowerCase());
    out[key] = v !== null && typeof v === "object" && !Array.isArray(v) ? snake(v as Record<string, unknown>) : v;
  }
  return out;
}

/** Converts LoggerOptions to the core's JSON config. */
export function toNativeConfig(o: LoggerOptions): Record<string, unknown> {
  const { libPath: _l, useContext: _u, handleSignals: _h, compress, console: con, ...rest } = o;
  const cfg = snake(rest as Record<string, unknown>);
  if (compress !== undefined) {
    cfg.compress = typeof compress === "boolean" ? { enabled: compress } : snake(compress as Record<string, unknown>);
  }
  if (con !== undefined) cfg.console = con === true ? "stderr" : con === false ? "off" : con;
  return cfg;
}

// ---- core ----------------------------------------------------------------

// Process-wide registry (survives `bun --hot` module reloads): one native
// stream per (dir, name), shared by every createLogger call that names it.
const registry: { cores: Map<string, Core>; hooks: boolean } = ((globalThis as Record<symbol, unknown>)[
  Symbol.for("rotlog.registry")
] ??= { cores: new Map<string, Core>(), hooks: false }) as { cores: Map<string, Core>; hooks: boolean };
/** Snapshot (closing a core removes it from the map). */
const openCores = () => [...registry.cores.values()];

function streamKey(opts: LoggerOptions): string {
  return `${resolve(opts.dir ?? "logs")}\0${opts.name ?? "app"}`;
}

function installHooks(signals: boolean) {
  if (registry.hooks) return;
  registry.hooks = true;
  process.on("exit", () => {
    for (const c of openCores()) c.close();
  });
  if (!signals) return;
  for (const sig of ["SIGINT", "SIGTERM"] as const) {
    const handler = () => {
      for (const c of openCores()) c.flush();
      // Only exit ourselves if nobody else is handling the signal.
      if (process.listenerCount(sig) === 1) {
        for (const c of openCores()) c.close();
        process.exit(sig === "SIGINT" ? 130 : 143);
      }
    };
    process.on(sig, handler);
  }
  process.on("SIGHUP", () => {
    for (const c of openCores()) c.rotate();
  });
}

/** One native stream shared by a logger and all its children. */
class Core {
  lib: Native;
  h: Pointer | null;
  /** Live view of the native level byte (also changed by `rotlog level`). */
  lv: Uint8Array;
  buf: Buffer = Buffer.allocUnsafe(64 * 1024);
  statBuf: Buffer = Buffer.allocUnsafe(1024);
  useContext: boolean;
  name: string;
  key: string;

  constructor(opts: LoggerOptions) {
    this.lib = native(opts.libPath);
    const json = Buffer.from(JSON.stringify(toNativeConfig(opts)));
    const h = this.lib.rl_open(json, json.length) as Pointer | null;
    if (!h) throw new Error(`rotlog: ${lastError(this.lib)}`);
    this.h = h;
    const lp = this.lib.rl_level_ptr(h) as Pointer | null;
    this.lv = lp ? new Uint8Array(toArrayBuffer(lp, 0, 1)) : new Uint8Array([L[opts.level ?? "info"]]);
    this.useContext = opts.useContext ?? true;
    this.name = opts.name ?? "app";
    this.key = streamKey(opts);
    registry.cores.set(this.key, this);
    installHooks(opts.handleSignals ?? true);
  }

  send(level: number, name: Uint8Array, msg: string, fields: string): void {
    const h = this.h;
    if (!h) return;
    let buf = this.buf;
    const need = name.length + (msg.length + fields.length) * 3;
    if (need > buf.length) buf = this.buf = Buffer.allocUnsafe(Math.max(need, buf.length * 2));
    buf.set(name, 0);
    const ml = buf.write(msg, name.length, "utf8");
    const fl = fields.length ? buf.write(fields, name.length + ml, "utf8") : 0;
    this.lib.rl_log(h, level, buf, name.length, ml, fl);
  }

  flush(): void {
    if (this.h && this.lib.rl_flush(this.h) !== 0) throw new Error(`rotlog: ${lastError(this.lib)}`);
  }

  rotate(): boolean {
    if (!this.h) return false;
    const r = this.lib.rl_rotate(this.h);
    if (r < 0) throw new Error(`rotlog: ${lastError(this.lib)}`);
    return r === 1;
  }

  stats(): Stats {
    if (!this.h) throw new Error("rotlog: logger is closed");
    let n = this.lib.rl_stats(this.h, this.statBuf, this.statBuf.length);
    if (n > this.statBuf.length) {
      this.statBuf = Buffer.allocUnsafe(n * 2);
      n = this.lib.rl_stats(this.h, this.statBuf, this.statBuf.length);
    }
    return JSON.parse(this.statBuf.toString("utf8", 0, n)) as Stats;
  }

  close(): void {
    const h = this.h;
    if (!h) return;
    this.h = null;
    // The native byte is freed by rl_close: switch to a JS-owned "off" level first.
    this.lv = new Uint8Array([L.off]);
    if (registry.cores.get(this.key) === this) registry.cores.delete(this.key);
    this.lib.rl_close(h);
  }
}

// ---- logger ---------------------------------------------------------------

const EMPTY = new Uint8Array(0);

export class Logger {
  /** @internal */ readonly core: Core;
  /** Logger name written with each record. */
  readonly name: string;
  private readonly nameBytes: Uint8Array;
  /** Bound fields as a JSON object body without braces. */
  private readonly bound: string;

  /** @internal */
  constructor(core: Core, name: string | undefined, bound: string) {
    this.core = core;
    this.name = name ?? core.name;
    this.nameBytes = name === undefined ? EMPTY : Buffer.from(name);
    this.bound = bound;
  }

  get level(): Level {
    return LEVELS[this.core.lv[0]!] ?? "off";
  }

  set level(level: Level) {
    if (this.core.h) this.core.lib.rl_set_level(this.core.h, L[level]);
  }

  isEnabled(level: Level): boolean {
    return L[level] >= this.core.lv[0]! && level !== "off";
  }

  trace(msg: string | Error, fields?: Fields | Error): void {
    if (L.trace >= this.core.lv[0]!) this.emit(L.trace, msg, fields);
  }
  debug(msg: string | Error, fields?: Fields | Error): void {
    if (L.debug >= this.core.lv[0]!) this.emit(L.debug, msg, fields);
  }
  info(msg: string | Error, fields?: Fields | Error): void {
    if (L.info >= this.core.lv[0]!) this.emit(L.info, msg, fields);
  }
  warn(msg: string | Error, fields?: Fields | Error): void {
    if (L.warn >= this.core.lv[0]!) this.emit(L.warn, msg, fields);
  }
  error(msg: string | Error, fields?: Fields | Error): void {
    if (L.error >= this.core.lv[0]!) this.emit(L.error, msg, fields);
  }
  fatal(msg: string | Error, fields?: Fields | Error): void {
    if (L.fatal >= this.core.lv[0]!) this.emit(L.fatal, msg, fields);
  }

  log(level: Level, msg: string | Error, fields?: Fields | Error): void {
    const n = L[level];
    if (n !== L.off && n >= this.core.lv[0]!) this.emit(n, msg, fields);
  }

  /**
   * Logs an HTTP request: error for 5xx, warn for 4xx, info otherwise.
   * Fields: method, path, status, dur_ms, ip, user, req_id, plus any extras.
   */
  request(info: RequestInfo, err?: unknown): void {
    const { method, path, status, durationMs, ip, user, reqId, ...extra } = info;
    const level = status >= 500 ? L.error : status >= 400 ? L.warn : L.info;
    if (level < this.core.lv[0]!) return;
    const f: Fields = { method, path, status };
    if (durationMs !== undefined) f.dur_ms = Math.round(durationMs * 1000) / 1000;
    if (ip !== undefined) f.ip = ip;
    const u = user ?? als.getStore()?.get("user");
    if (u !== undefined) f.user = u;
    if (reqId !== undefined) f.req_id = reqId;
    for (const k in extra) if (extra[k] !== undefined) f[k] = extra[k];
    if (err !== undefined) f.err = serializeError(err);
    // The request's own fields are explicit here; skip the context to avoid duplicates.
    this.emit(level, `${method} ${path} ${status}`, f, false);
  }

  /** A logger adding `fields` to every record (and optionally another name). */
  child(fields?: Fields, name?: string): Logger {
    const extra = fields ? innerJson(fields) : "";
    const bound = this.bound && extra ? `${this.bound},${extra}` : this.bound || extra;
    return new Logger(this.core, name ?? (this.nameBytes.length ? this.name : undefined), bound);
  }

  /** Same stream, different logger name (`log.named("db")`). */
  named(name: string): Logger {
    return new Logger(this.core, name, this.bound);
  }

  /** Blocks until everything logged so far is written to disk. */
  flush(): void {
    this.core.flush();
  }

  /** Archives the active file now. Returns false if it was empty. */
  rotate(): boolean {
    return this.core.rotate();
  }

  /**
   * Runs compression, upload and retention now, resolving when done.
   * Never blocks the event loop.
   */
  async maintain(): Promise<Stats> {
    const c = this.core;
    if (!c.h) throw new Error("rotlog: logger is closed");
    const ticket = c.lib.rl_maintain(c.h, 0);
    for (;;) {
      const s = c.stats();
      if (ticket === 0 || s.maintenance_completed >= ticket) return s;
      await Bun.sleep(20);
    }
  }

  stats(): Stats {
    return this.core.stats();
  }

  /** Flushes and closes the stream (all loggers sharing it become no-ops). */
  close(): void {
    this.core.close();
  }

  /** Path of the loaded native library. */
  static get nativeLibrary(): string | undefined {
    return nativePath();
  }

  private emit(level: number, msg: string | Error, fields: Fields | Error | undefined, ctx = true): void {
    let m: string;
    let fj = "";
    if (typeof msg === "string") {
      m = msg;
    } else {
      m = msg instanceof Error ? msg.message || msg.name : String(msg);
      fj = innerJson({ err: serializeError(msg) });
    }
    if (fields !== undefined) {
      const s = fields instanceof Error ? innerJson({ err: serializeError(fields) }) : innerJson(fields);
      if (s) fj = fj ? `${fj},${s}` : s;
    }
    let all = this.bound;
    if (ctx && this.core.useContext) {
      const store = als.getStore();
      if (store !== undefined) {
        const c = store.inner();
        if (c) all = all ? `${all},${c}` : c;
      }
    }
    if (fj) all = all ? `${all},${fj}` : fj;
    this.core.send(level, this.nameBytes, m, all ? `{${all}}` : "");
  }
}

/**
 * Opens (or creates) a log stream. Only one process may write a given stream;
 * within a process, later calls for the same dir + name share the open stream
 * (its first options win), which also keeps `bun --hot` reloads working.
 */
export function createLogger(opts: LoggerOptions = {}): Logger {
  const existing = registry.cores.get(streamKey(opts));
  if (existing?.h) {
    if (opts.useContext !== undefined) existing.useContext = opts.useContext;
    return new Logger(existing, undefined, "");
  }
  return new Logger(new Core(opts), undefined, "");
}

// ---- Bun.serve integration -------------------------------------------------

type ServerLike = { requestIP?(req: Request): { address: string } | null } | undefined;
type Handler<S> = (req: Request, server: S) => Response | Promise<Response>;

export interface RequestLoggingOptions {
  /** Header carrying the request id (read, generated if absent, echoed on the response). Default "x-request-id". */
  requestIdHeader?: string;
  /** Resolve the user for a request (or call `setUser()` inside the handler). */
  getUser?: (req: Request) => string | undefined | Promise<string | undefined>;
  /** Skip logging for some requests (health checks…). */
  skip?: (req: Request) => boolean;
  /** Also log the user agent as `ua`. */
  userAgent?: boolean;
}

function pathOf(url: string): string {
  // Faster than new URL(): "http://host:port/path?query" -> "/path?query" minus the query.
  const start = url.indexOf("/", url.indexOf("//") + 2);
  if (start < 0) return "/";
  const q = url.indexOf("?", start);
  return q < 0 ? url.slice(start) : url.slice(start, q);
}

/**
 * Wraps a Bun.serve `fetch` handler: logs one line per request (level from
 * status), puts req_id/method/path/user into every record logged while
 * handling it, logs thrown errors with their stack (then rethrows), and echoes
 * the request id header.
 */
export function withRequestLogging<S extends ServerLike>(
  log: Logger,
  handler: Handler<S>,
  opts: RequestLoggingOptions = {},
): Handler<S> {
  const header = (opts.requestIdHeader ?? "x-request-id").toLowerCase();
  return (req: Request, server: S) => {
    if (opts.skip?.(req)) return handler(req, server);
    const start = performance.now();
    const method = req.method;
    const path = pathOf(req.url);
    const reqId = req.headers.get(header) ?? crypto.randomUUID();
    const ctx = new RequestContext({ req_id: reqId, method, path });
    let ip: string | undefined;
    try {
      ip = server?.requestIP?.(req)?.address;
    } catch {
      ip = undefined;
    }
    const extra: Fields = {};
    if (opts.userAgent) extra.ua = req.headers.get("user-agent") ?? undefined;
    return als.run(ctx, async () => {
      if (opts.getUser) {
        const u = await opts.getUser(req);
        if (u !== undefined) ctx.set("user", u);
      }
      let res: Response;
      try {
        res = await handler(req, server);
      } catch (err) {
        log.request({ method, path, status: 500, durationMs: performance.now() - start, ip, reqId, user: ctx.get("user") as string | undefined, ...extra }, err);
        throw err;
      }
      log.request({ method, path, status: res.status, durationMs: performance.now() - start, ip, reqId, user: ctx.get("user") as string | undefined, ...extra });
      try {
        res.headers.set(header, reqId);
      } catch {
        // Immutable headers (e.g. a proxied fetch Response): leave as is.
      }
      return res;
    });
  };
}

type RouteValue = unknown;

/**
 * Wraps every function handler of a Bun.serve `routes` object
 * (including per-method `{ GET, POST }` maps) with request logging.
 */
export function logRoutes<R extends Record<string, RouteValue>>(log: Logger, routes: R, opts: RequestLoggingOptions = {}): R {
  const out: Record<string, RouteValue> = {};
  for (const [path, v] of Object.entries(routes)) {
    if (typeof v === "function") {
      out[path] = withRequestLogging(log, v as Handler<ServerLike>, opts);
    } else if (v && typeof v === "object" && !(v instanceof Response)) {
      const methods: Record<string, RouteValue> = {};
      for (const [m, h] of Object.entries(v)) {
        methods[m] = typeof h === "function" ? withRequestLogging(log, h as Handler<ServerLike>, opts) : h;
      }
      out[path] = methods;
    } else {
      out[path] = v;
    }
  }
  return out as R;
}

export default createLogger;
