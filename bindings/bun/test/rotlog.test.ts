import { afterAll, expect, test } from "bun:test";
import { generateKeyPairSync, verify } from "node:crypto";
import { existsSync, mkdtempSync, readdirSync, readFileSync, rmSync, unlinkSync, writeFileSync } from "node:fs";
import { hostname, tmpdir } from "node:os";
import { join } from "node:path";
import { createLogger, logRoutes, setUser, withContext, withRequestLogging } from "../src/index";

const dirs: string[] = [];
const tmp = () => {
  const d = mkdtempSync(join(tmpdir(), "rotlog-bun-"));
  dirs.push(d);
  return d;
};
afterAll(() => {
  for (const d of dirs) rmSync(d, { recursive: true, force: true });
});

const jsonLines = (p: string) =>
  readFileSync(p, "utf8")
    .split("\n")
    .filter(Boolean)
    .map((l) => JSON.parse(l));

const CLI = join(import.meta.dir, "..", "..", "..", "target", "release", "rotlog");

test("records, fields, children, errors and level filtering", () => {
  const dir = tmp();
  const log = createLogger({ dir, name: "t", format: "json", level: "debug", compress: false, maintenanceInterval: 0 });
  log.info("hello", { user: "alice", n: 1, nested: { a: [1, 2] } });
  log.trace("hidden");
  const db = log.child({ pool: "main" }, "db");
  db.debug("query", { ms: 1.5 });
  db.child({ shard: 2 }).info("deeper");
  log.error(new Error("boom"));
  log.warn("with err", new TypeError("bad input"));
  log.info('unicode ✓ "quotes"\nnewline', { big: 10n });
  withContext({ job: "nightly" }, () => log.info("in context"));
  log.flush();

  const r = jsonLines(join(dir, "t.log"));
  expect(r.map((x) => x.msg)).toEqual(["hello", "query", "deeper", "boom", "with err", 'unicode ✓ "quotes"\nnewline', "in context"]);
  expect(r[0]).toMatchObject({ level: "info", logger: "t", user: "alice", n: 1, nested: { a: [1, 2] } });
  expect(r[1]).toMatchObject({ level: "debug", logger: "db", pool: "main", ms: 1.5 });
  expect(r[2]).toMatchObject({ logger: "db", pool: "main", shard: 2 });
  expect(r[3].level).toBe("error");
  expect(r[3].err.name).toBe("Error");
  expect(r[3].err.stack).toContain("rotlog.test.ts");
  expect(r[4].err).toMatchObject({ name: "TypeError", message: "bad input" });
  expect(r[5].big).toBe("10");
  expect(r[6].job).toBe("nightly");
  // Microsecond RFC 3339 timestamps, non-decreasing.
  for (let i = 1; i < r.length; i++) expect(Date.parse(r[i].ts) >= Date.parse(r[i - 1].ts)).toBe(true);
  expect(r[0].ts).toMatch(/^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{6}([+-]\d\d:\d\d|Z)$/);
  log.close();
  log.info("after close is a no-op");
});

test("createLogger reuses an open stream in the same process (bun --hot)", () => {
  const dir = tmp();
  const a = createLogger({ dir, name: "same", format: "json", compress: false, maintenanceInterval: 0 });
  const b = createLogger({ dir, name: "same" });
  expect(b.core).toBe(a.core);
  a.info("from a");
  b.info("from b");
  b.flush();
  expect(jsonLines(join(dir, "same.log")).map((x) => x.msg)).toEqual(["from a", "from b"]);
  a.close();
  const c = createLogger({ dir, name: "same", format: "json", compress: false, maintenanceInterval: 0 });
  expect(c.core).not.toBe(a.core);
  c.close();
});

test("text format writes logfmt fields", () => {
  const dir = tmp();
  const log = createLogger({ dir, name: "t", compress: false, maintenanceInterval: 0, timezone: "utc" });
  log.warn("slow query", { ms: 812, sql: "select 1", ok: false });
  log.flush();
  const line = readFileSync(join(dir, "t.log"), "utf8").trim();
  expect(line).toMatch(/^\S+Z WARN  t: slow query\tms=812 sql="select 1" ok=false$/);
  log.close();
});

test("level changes at runtime, including from the CLI", () => {
  const dir = tmp();
  const log = createLogger({ dir, name: "lv", format: "json", compress: false, maintenanceInterval: 0 });
  expect(log.level).toBe("info");
  log.debug("dropped");
  log.level = "warn";
  log.info("dropped too");
  log.warn("kept");
  if (existsSync(CLI)) {
    const r = Bun.spawnSync([CLI, "-d", dir, "-N", "lv", "level", "debug"]);
    expect(r.stdout.toString().trim()).toBe("level=debug");
    expect(log.level as string).toBe("debug"); // seen through the shared level byte, no call needed
    log.debug("now visible");
  }
  log.flush();
  const msgs = jsonLines(join(dir, "lv.log")).map((x) => x.msg);
  expect(msgs).toEqual(existsSync(CLI) ? ["kept", "now visible"] : ["kept"]);
  log.close();
});

test("Bun.serve request logging with per-request context", async () => {
  const dir = tmp();
  const log = createLogger({ dir, name: "http", format: "json", compress: false, maintenanceInterval: 0 });
  const server = Bun.serve({
    port: 0,
    fetch: withRequestLogging(log, async (req) => {
      const path = new URL(req.url).pathname;
      if (path === "/boom") throw new Error("kaboom");
      if (path.startsWith("/users/")) {
        setUser("u42");
        await Bun.sleep(5);
        log.info("loading profile");
        return Response.json({ ok: true });
      }
      return new Response("nope", { status: 404 });
    }),
    error: () => new Response("internal", { status: 500 }),
  });
  let generated: string | null = null;
  try {
    const ok = await fetch(`${server.url}users/7?x=1`, { headers: { "x-request-id": "abc-123" } });
    expect(ok.headers.get("x-request-id")).toBe("abc-123");
    await ok.text();
    const nf = await fetch(`${server.url}missing`);
    expect(nf.status).toBe(404);
    generated = nf.headers.get("x-request-id");
    expect(generated).toMatch(/^[0-9a-f-]{36}$/);
    const boom = await fetch(`${server.url}boom`);
    expect(boom.status).toBe(500);
  } finally {
    server.stop(true);
  }
  log.flush();
  const r = jsonLines(join(dir, "http.log"));
  const inner = r.find((x) => x.msg === "loading profile");
  expect(inner).toMatchObject({ level: "info", req_id: "abc-123", method: "GET", path: "/users/7", user: "u42" });
  const okLine = r.find((x) => x.msg === "GET /users/7 200");
  expect(okLine).toMatchObject({ level: "info", status: 200, user: "u42", req_id: "abc-123" });
  expect(okLine.dur_ms).toBeGreaterThan(4);
  expect(okLine.ip).toBeTruthy();
  expect(r.find((x) => x.msg === "GET /missing 404")).toMatchObject({ level: "warn", req_id: generated });
  const err = r.find((x) => x.msg === "GET /boom 500");
  expect(err).toMatchObject({ level: "error", status: 500 });
  expect(err.err.message).toBe("kaboom");
  expect(err.err.stack).toContain("kaboom");
  log.close();
});

test("logRoutes wraps route handlers", async () => {
  const dir = tmp();
  const log = createLogger({ dir, name: "routes", format: "json", compress: false, maintenanceInterval: 0 });
  const server = Bun.serve({
    port: 0,
    routes: logRoutes(log, {
      "/health": new Response("ok"),
      "/items/:id": { GET: () => Response.json({}), DELETE: () => new Response(null, { status: 403 }) },
    }),
    fetch: () => new Response("x"),
  });
  try {
    await (await fetch(`${server.url}items/1`)).text();
    await (await fetch(`${server.url}items/1`, { method: "DELETE" })).text();
    await (await fetch(`${server.url}health`)).text();
  } finally {
    server.stop(true);
  }
  log.flush();
  const msgs = jsonLines(join(dir, "routes.log")).map((x) => `${x.level} ${x.msg}`);
  expect(msgs).toEqual(["info GET /items/1 200", "warn DELETE /items/1 403"]);
  log.close();
});

test("rotation and gzip", async () => {
  const dir = tmp();
  const log = createLogger({ dir, name: "rot", compress: { after: 0 } });
  log.info("first file");
  expect(log.rotate()).toBe(true);
  expect(log.rotate()).toBe(false); // empty
  log.info("second file");
  const s = await log.maintain();
  expect(s.rotations).toBe(1);
  expect(s.compressed).toBe(1);
  const gz = readdirSync(dir).filter((f) => f.endsWith(".log.gz"));
  expect(gz.length).toBe(1);
  expect(new TextDecoder().decode(Bun.gunzipSync(readFileSync(join(dir, gz[0]!))))).toContain("first file");
  log.close();
});

test("uploads archives to Google Cloud Storage (mock) with a signed service-account JWT", async () => {
  const dir = tmp();
  const { privateKey, publicKey } = generateKeyPairSync("rsa", {
    modulusLength: 2048,
    privateKeyEncoding: { type: "pkcs8", format: "pem" },
    publicKeyEncoding: { type: "spki", format: "pem" },
  });
  const objects = new Map<string, Uint8Array<ArrayBuffer>>();
  const seen: string[] = [];
  const b64 = (s: string) => Buffer.from(s, "base64url");
  const mock = Bun.serve({
    port: 0,
    async fetch(req) {
      const url = new URL(req.url);
      seen.push(`${req.method} ${url.pathname}`);
      if (url.pathname === "/token") {
        const form = new URLSearchParams(await req.text());
        expect(form.get("grant_type")).toBe("urn:ietf:params:oauth:grant-type:jwt-bearer");
        const [h, c, sig] = form.get("assertion")!.split(".");
        const header = JSON.parse(b64(h!).toString());
        const claims = JSON.parse(b64(c!).toString());
        const valid = verify("RSA-SHA256", Buffer.from(`${h}.${c}`), publicKey, b64(sig!));
        if (!valid || header.kid !== "kid-1" || claims.iss !== "uploader@test.iam.gserviceaccount.com") {
          return new Response("bad jwt", { status: 401 });
        }
        expect(claims.scope).toBe("https://www.googleapis.com/auth/devstorage.read_write");
        expect(claims.aud).toBe(`${mockUrl}/token`);
        return Response.json({ access_token: "tok-123", expires_in: 3600, token_type: "Bearer" });
      }
      if (req.headers.get("authorization") !== "Bearer tok-123") return new Response("unauthorized", { status: 401 });
      const m = url.pathname.match(/^\/upload\/storage\/v1\/b\/([^/]+)\/o$/);
      if (m && req.method === "POST") {
        expect(m[1]).toBe("test-bucket");
        expect(url.searchParams.get("ifGenerationMatch")).toBe("0");
        expect(req.headers.get("content-type")).toBe("application/gzip");
        const name = url.searchParams.get("name")!;
        if (objects.has(name)) return new Response("exists", { status: 412 });
        const body = new Uint8Array(await req.arrayBuffer());
        objects.set(name, body);
        return Response.json({ name, size: String(body.length) });
      }
      const g = url.pathname.match(/^\/storage\/v1\/b\/test-bucket\/o\/(.+)$/);
      if (g && req.method === "GET") {
        const obj = objects.get(decodeURIComponent(g[1]!));
        return obj ? Response.json({ size: String(obj.length) }) : new Response("nf", { status: 404 });
      }
      return new Response("unexpected", { status: 400 });
    },
  });
  const mockUrl = `http://127.0.0.1:${mock.port}`;
  const credentials = join(dir, "sa.json");
  writeFileSync(
    credentials,
    JSON.stringify({
      type: "service_account",
      client_email: "uploader@test.iam.gserviceaccount.com",
      private_key: privateKey,
      private_key_id: "kid-1",
      token_uri: `${mockUrl}/token`,
    }),
  );
  const logDir = join(dir, "logs");
  const log = createLogger({
    dir: logDir,
    name: "api",
    upload: { bucket: "test-bucket", prefix: "prod/", credentials, endpoint: mockUrl },
  });
  try {
    for (let i = 0; i < 100; i++) log.info("request served", { i });
    expect(log.rotate()).toBe(true);
    let s = await log.maintain();
    expect(s.last_error).toBeNull();
    expect(s.compressed).toBe(1);
    expect(s.uploaded).toBe(1);
    expect(objects.size).toBe(1);
    const [name, body] = [...objects][0]!;
    const archive = readdirSync(logDir).find((f) => f.endsWith(".log.gz"))!;
    const [yyyy, mm, dd] = archive.slice(4, 14).split("-");
    expect(name).toBe(`prod/${hostname()}/api/${yyyy}/${mm}/${dd}/${archive}`);
    const text = new TextDecoder().decode(Bun.gunzipSync(body));
    expect(text.split("\n").filter((l) => l.includes("request served")).length).toBe(100);

    // Crash between upload and bookkeeping: the retry sees 412 and verifies the size instead of re-uploading.
    unlinkSync(join(logDir, ".api.uploaded"));
    s = await log.maintain();
    expect(s.uploaded).toBe(2);
    expect(s.upload_errors).toBe(0);
    expect(objects.size).toBe(1);
    expect(seen.filter((x) => x.startsWith("GET /storage")).length).toBe(1);
    expect(seen.filter((x) => x === "POST /token").length).toBe(1); // token cached
  } finally {
    log.close();
    mock.stop(true);
  }
});
