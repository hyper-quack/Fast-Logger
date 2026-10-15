# rotlog for Bun

A rotating logger for Bun that runs on a Rust core loaded through `bun:ffi`. It rotates files by time or size, gzips archives, uploads them to Google Cloud Storage, and logs HTTP requests with a per-request context.

```sh
bun run build:native   # cargo build --release, then copies librotlog into ./native
bun test
```

```ts
import { createLogger, withRequestLogging, setUser } from "rotlog";

const log = createLogger({ dir: "logs", name: "api", format: "json", upload: { bucket: "my-logs" } });

Bun.serve({
  fetch: withRequestLogging(log, (req) => {
    setUser("alice");
    log.info("handling");           // includes req_id, method, path, user
    return new Response("ok");
  }),
});
```

See the [main README](../../README.md#bun--typescript) for the full API, the configuration keys and the `rotlog` CLI.
