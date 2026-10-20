// bun run examples/server.ts   then   curl localhost:3000/users/42 -H 'x-user: alice'
// Logs go to <repo root>/logs. From the repo root:
//   ./target/release/rotlog -N api tail -f      ./target/release/rotlog -N api stats
//
// Environment (all optional):
//   PORT=3000  LOG_DIR=<repo>/logs  LOG_LEVEL=info  LOG_FORMAT=json|text
//   LOG_EVERY=daily (try 30s for a live demo)  LOG_COMPRESS_AFTER=1h (try 0s)
//   LOG_BUCKET=my-bucket  LOG_PREFIX=prod/  GOOGLE_APPLICATION_CREDENTIALS=sa.json
import { join } from "node:path";
import { createLogger, setUser, withRequestLogging, type Level } from "../src/index";

/** `logs/` at the repository root, whatever directory the server is started from. */
const ROOT_LOGS = join(import.meta.dir, "..", "..", "..", "logs");

const env = process.env;
const log = createLogger({
  dir: env.LOG_DIR ?? ROOT_LOGS,
  name: "api",
  format: env.LOG_FORMAT === "text" ? "text" : "json",
  level: (env.LOG_LEVEL as Level | undefined) ?? "info",
  console: env.NODE_ENV !== "production",
  rotation: { every: env.LOG_EVERY ?? "daily", maxSize: "200MB", keep: 14 },
  compress: { after: env.LOG_COMPRESS_AFTER ?? "1h" },
  maintenanceInterval: env.LOG_EVERY ? "5s" : "1m",
  upload: env.LOG_BUCKET ? { bucket: env.LOG_BUCKET, prefix: env.LOG_PREFIX ?? "prod/" } : undefined,
});
const db = log.named("db");

const server = Bun.serve({
  port: Number(process.env.PORT ?? 3000),
  fetch: withRequestLogging(log, async (req) => {
    const url = new URL(req.url);
    const user = req.headers.get("x-user");
    if (user) setUser(user); // appears on every record of this request

    const m = url.pathname.match(/^\/users\/(\d+)$/);
    if (m) {
      const started = performance.now();
      await Bun.sleep(Math.random() * 20); // pretend to query a database
      const row = { id: Number(m[1]), name: "Ada" };
      db.debug("select user", { id: row.id, ms: performance.now() - started }); // carries req_id/user too
      return Response.json(row);
    }
    if (url.pathname === "/slow") {
      await Bun.sleep(100 + Math.random() * 300);
      log.warn("slow upstream", { upstream: "billing" });
      return new Response("finally");
    }
    if (url.pathname === "/boom") throw new Error("database unavailable");
    return new Response("not found", { status: 404 });
  }),
  error: () => new Response("internal error", { status: 500 }),
});

log.info("listening", { url: server.url.href, pid: process.pid });
