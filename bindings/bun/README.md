# rotlog

A fast rotating logger for Bun, running on a Rust core through `bun:ffi`:

- time- and size-based rotation
- gzip archiving
- Google Cloud Storage upload
- retention
- request logging with a per-request context
- the `rotlog` CLI to tail, query and summarize logs

```sh
bun add rotlog
```

Prebuilt for macOS (arm64, x64) and Linux (x64, arm64, glibc). Bun only: the package ships TypeScript and uses `bun:ffi`.

```ts
import { createLogger, withRequestLogging, setUser } from "rotlog";

const log = createLogger({
  dir: "logs",
  name: "api",
  format: "json",
  rotation: { every: "daily", maxSize: "200MB", keep: 14 },
  compress: { after: "1h" },
  upload: { bucket: "my-logs", prefix: "prod/" },   // optional
});

log.info("server started", { port: 3000 });
log.error(new Error("payment failed"));

Bun.serve({
  fetch: withRequestLogging(log, (req) => {
    setUser("alice");
    log.info("handling");           // includes req_id, method, path, user
    return new Response("ok");
  }),
});
```

The CLI comes with the package:

```sh
bunx rotlog -N api tail -f -l warn
bunx rotlog -N api query --since 2h --status 5xx --user alice
bunx rotlog -N api stats
bunx rotlog -N api level debug      # change a running app's level
```

Full documentation (options, CLI reference, log format, GCS setup): https://github.com/hyper-quack/fast-logger#readme
