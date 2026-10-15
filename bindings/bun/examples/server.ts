// bun run examples/server.ts   then   curl localhost:3000/users/42 -H 'x-user: alice'
// Inspect with:  rotlog -d logs -N api tail -f      rotlog -d logs -N api stats
import { createLogger, setUser, withRequestLogging } from "../src/index";

const log = createLogger({
  dir: "logs",
  name: "api",
  format: "json",
  level: (process.env.LOG_LEVEL as "debug" | undefined) ?? "info",
  console: process.env.NODE_ENV !== "production",
  rotation: { every: "daily", maxSize: "200MB", keep: 14 },
  compress: { after: "1h" },
  upload: process.env.LOG_BUCKET ? { bucket: process.env.LOG_BUCKET, prefix: "prod/", deleteLocal: true } : undefined,
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
      const row = { id: Number(m[1]), name: "Ada" };
      db.debug("select user", { id: row.id, ms: performance.now() - started }); // carries req_id/user too
      return Response.json(row);
    }
    if (url.pathname === "/boom") throw new Error("database unavailable");
    return new Response("not found", { status: 404 });
  }),
  error: () => new Response("internal error", { status: 500 }),
});

log.info("listening", { url: server.url.href, pid: process.pid });
