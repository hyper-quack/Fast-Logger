# rotlog

A small, fast rotating logger written in Rust, with a command line tool and a TypeScript API for Bun.

- **Rotation** by time (aligned to the wall clock: hourly at :00, daily at midnight, `6h`, `weekly:sun`, …) and/or size.
- **Accurate**: microsecond RFC 3339 timestamps taken under the writer lock, so file order equals time order and every record lands in the file of the period it was stamped in. Nothing is dropped by default.
- **Compression**: archives are gzipped after a configurable delay.
- **Upload** to Google Cloud Storage, idempotent and crash-safe. Local archives are never deleted before they are uploaded.
- **Retention**: keep N archives and/or a maximum age.
- **Request logging**: method, path, status, duration, IP, user and request id. The level comes from the status (5xx → error, 4xx → warn). In Bun, a per-request context adds `req_id` and `user` to every record logged while handling the request.
- **CLI for everything**: write, pipe, wrap a process, tail, query, stats, rotate, compress, upload, prune. It can also change the level of a running process.
- **Fast**: ~170 ns per record in Rust, ~350 ns per call from Bun, 10–15 ns for a filtered-out call. Records are written to disk in large batches on a background thread.

```
2026-10-15T14:11:44.914102+01:00 INFO  api: GET /users/42 200	method=GET path=/users/42 status=200 dur_ms=6.32 ip=::1 user=alice req_id=0727…
```

---

## Contents

- [Install](#install)
- [Build from source](#build-from-source)
- [CLI quick start](#cli-quick-start)
- [Bun / TypeScript](#bun--typescript)
- [Rust library](#rust-library)
- [CLI reference](#cli-reference)
- [Configuration](#configuration)
- [Log format](#log-format)
- [Rotation, compression, retention and upload](#rotation-compression-retention-and-upload)
- [Google Cloud Storage setup](#google-cloud-storage-setup)
- [Controlling a running writer](#controlling-a-running-writer)
- [How it works](#how-it-works)
- [Performance](#performance)
- [Development](#development)
- [Releasing](#releasing)

## Install

```sh
bun add rotlog          # Bun library + `rotlog` CLI (bunx rotlog …), prebuilt for macOS and Linux
cargo install rotlog    # CLI only, built from source (Rust 1.88+)
```

Prebuilt CLI binaries for macOS (arm64, x64) and Linux (x64, arm64; glibc 2.17+) are also attached to each [GitHub release](https://github.com/hyper-quack/fast-logger/releases). Each archive contains `rotlog` and `librotlog`.

The npm package is for Bun: it uses `bun:ffi` and ships TypeScript sources. Its native part comes from an optional dependency matching your platform (`rotlog-darwin-arm64`, `rotlog-linux-x64-gnu`, …). Windows and musl (Alpine) are not supported yet.

## Build from source

You need Rust 1.88 or newer.

```sh
cargo build --release
# target/release/rotlog                 the CLI
# target/release/librotlog.{dylib,so}   the shared library used by Bun
cp target/release/rotlog /usr/local/bin/
```

Cargo features: `cli` (the binary) and `gcs` (uploads) are both on by default. `cargo build --release --no-default-features --lib` builds the core alone, without TLS or HTTP.

## CLI quick start

```sh
rotlog init                                   # writes a commented rotlog.toml (optional)

# 1. Wrap any program: stdout → info, stderr → error, levels detected from content
rotlog -N api --every hourly --bucket my-logs run -- bun server.ts

# 2. Or pipe into it
bun server.ts 2>&1 | rotlog -N api pipe

# 3. Write from scripts / cron
rotlog -N backup write -l error "backup failed" -f job=nightly -f code=3

# Read
rotlog -N api tail -f -l warn                 # follow, warnings and up
rotlog -N api query --since 2h --status 5xx --user alice
rotlog -N api query -w 'dur_ms>500' -w path~/checkout --json
rotlog -N api stats --since 1d                # levels, status classes, latency p50–p99, top routes/users/errors

# Operate
rotlog -N api status                          # writer pid, counters, files, policy
rotlog -N api level debug                     # change the running writer's level
rotlog -N api rotate | flush
rotlog -N api maintain                        # compress + upload + prune now
```

`-d/--dir` (default `logs`) and `-N/--name` (default `app`) select the stream: the active file is `<dir>/<name>.log`.

## Bun / TypeScript

Install with `bun add rotlog`. The package loads the Rust core through `bun:ffi`. Working from this repository instead:

```sh
cd bindings/bun && bun run build:native     # builds librotlog and copies it into ./native
```

The library is looked up in this order: the `libPath` option, `ROTLOG_LIB`, the installed platform package, `bindings/bun/native/`, then `target/release` and `target/debug`. The CLI (`bunx rotlog`) uses `ROTLOG_BIN`, then the same places.

```ts
import { createLogger, withRequestLogging, setUser } from "rotlog";

const log = createLogger({
  dir: "logs",
  name: "api",
  format: "json",                       // or "text" (default)
  level: "info",
  console: process.env.NODE_ENV !== "production",
  rotation: { every: "daily", maxSize: "200MB", keep: 14, maxAge: "30d" },
  compress: { after: "1h" },
  upload: { bucket: "my-logs", prefix: "prod/", deleteLocal: true },
});

log.info("server started", { port: 3000 });
log.warn("slow query", { ms: 812, sql: "select …" });
log.error(new Error("payment failed"));                 // msg = error message, err = { name, message, stack, cause }
log.error("charge failed", err);                        // or attach an error to a message

const db = log.named("db");                             // same file, logger name "db"
const billing = log.child({ service: "billing" });      // fields added to every record
log.level = "debug";                                    // runtime change (also: `rotlog level debug`)
```

### Request logging

```ts
Bun.serve({
  port: 3000,
  fetch: withRequestLogging(log, async (req) => {
    setUser(session.userId);                            // shows up on every record of this request
    db.debug("select user", { id });                    // carries req_id, method, path, user automatically
    return Response.json(row);
  }, {
    getUser: (req) => req.headers.get("x-user") ?? undefined,  // optional
    skip: (req) => new URL(req.url).pathname === "/health",   // optional
    requestIdHeader: "x-request-id",                            // read, generated if missing, echoed back
  }),
});
```

This writes one line per request, `GET /users/42 200`, with fields `method path status dur_ms ip user req_id`. The level is `error` for 5xx and thrown errors (which also get `err.stack`; the error is rethrown afterwards), `warn` for 4xx, and `info` otherwise. For the `routes` API, wrap the routes object with `logRoutes(log, routes)`. You can also log a request yourself with `log.request({ method, path, status, durationMs, user, ip, reqId })`, and attach fields to any piece of work with `withContext({ job: "nightly" }, fn)`.

### Lifecycle

| Call | Effect |
|---|---|
| `log.flush()` | Blocks until everything logged so far is on disk (local, fast). |
| `log.rotate()` | Archives the active file now; returns `false` if it was empty. |
| `await log.maintain()` | Compress, upload and prune now. Never blocks the event loop. |
| `log.stats()` | Counters: records, bytes, dropped, rotations, compressed, uploaded, upload_errors, last_error, … |
| `log.close()` | Flushes and closes the stream (runs automatically on process exit). |

- **Signals** (`handleSignals`, on by default): SIGINT and SIGTERM flush every stream, and exit only if no other handler is installed. SIGHUP rotates.
- **Within one process**, repeated `createLogger` calls for the same `dir` + `name` share the open stream, so `bun --hot` reloads keep working.
- **Across processes**, a stream has a single writer. Give each process or worker its own `name` (for example `api-${process.pid}` or `api-worker-1`).
- `error` and `fatal` records are written immediately. Other records wait at most `flushInterval` (default 100 ms).

Options are the camelCase form of the [configuration](#configuration) keys (`flushInterval`, `rotation.maxSize`, `upload.deleteLocal`, …), plus `libPath`, `useContext` and `handleSignals`.

## Rust library

```rust
use rotlog::{Config, Logger, Level, Request};
use std::time::Duration;

let log = Logger::open(Config { name: "api".into(), ..Config::default() })?;
log.info("server started", &[("port", 8080.into())]);

let db = log.named("db").with(&[("pool", "primary".into())]);
db.warn("slow query", &[("ms", 812.into())]);

log.request(&Request {
    method: "GET", path: "/users/42", status: 200,
    duration: Duration::from_micros(6320), user: Some("alice"), ..Default::default()
}, &[]);

log.log_json(Level::Error, Some("http"), "boom", r#"{"status":500}"#);
log.flush()?;   // the stream is also flushed and closed when the last handle is dropped
```

`Config` can be loaded with `Config::from_file("rotlog.toml")` or `Config::from_json(..)`.

## CLI reference

| Command | Purpose |
|---|---|
| `init [path] [--force]` | Write a commented `rotlog.toml`. |
| `config` | Print the effective configuration (file + env + flags). |
| `write [-l LEVEL] [--logger N] [-f k=v]… [--json-fields JSON] MESSAGE…` | Append one record. If another process owns the stream, the line is appended atomically and that process stays in charge of rotation. |
| `pipe [--default-level L] [--no-detect] [--tee] [--keep-ansi]` | Log stdin lines. Recognizes `ERROR`, `[warn]`, `level=debug` and JSON lines (`level`/`msg`, pino numeric levels). Strips ANSI colors. |
| `run [--stdout-level L] [--stderr-level L] [--tee] -- CMD…` | Run a command and log both of its output streams. Signals are forwarded and the exit code is propagated. Start and exit are logged. |
| `tail [-n N] [-f] [filters] [--raw\|--json]` | Last records, follow across rotations. |
| `query [filters] [--limit N] [--count] [--raw\|--json]` | Search the active file and every archive, gzip included, in time order. Exits 1 when nothing matches. |
| `stats [filters] [--top N] [--json]` | Levels, request status classes, latency percentiles, top routes (ids collapsed to `:id`), users, errors and loggers. |
| `ls [--all] [--json]` | Files with size, state (active / archive / gzip / uploaded) and start time. |
| `status [--json]` | Writer pid, live counters, files and policy. |
| `rotate`, `flush`, `level [LEVEL]` | Control the running writer (`rotate` works without one too). |
| `compress`, `upload [--check]`, `prune`, `maintain [--watch]` | Archive lifecycle; `--now` ignores delays, `--dry-run` only prints the plan. |
| `bench` | Throughput and latency on this machine. |

**Filters** (tail, query, stats): `-l/--level MIN`, `--logger`, `-g/--grep TEXT`, `--since`/`--until` (RFC 3339, `YYYY-MM-DD[ HH:MM]`, or an age like `15m`, `2h`, `7d`), `-w/--where` (repeatable: `status>=500`, `user=alice`, `path~/api`, `status=5xx`, `dur_ms>250`, `user.id=7`), and the shortcuts `--status`, `--user`, `--method`, `--path`, `--req`.

**Stream options** (global, they override the config file): `-c/--config`, `-d/--dir`, `-N/--name`, `--ext`, `--min-level`, `--format text|json`, `--tz local|utc`, `--every`, `--max-size`, `--keep`, `--max-age`, `--compress-after`, `--no-compress`, `--bucket`, `--prefix`, `--object`, `--upload-after`, `--delete-after-upload`, `--no-upload`, `--credentials`, `--endpoint`, `--console`, `--flush-interval`, `--fsync`, `--maintenance-interval`. The environment variables `ROTLOG_CONFIG`, `ROTLOG_DIR`, `ROTLOG_NAME` and `ROTLOG_BUCKET` are also read.

## Configuration

Settings are resolved in this order: `--config FILE`, then `./rotlog.toml` if it exists, then the defaults. Flags override all of these. JSON files work too, and the Bun API passes the same keys in camelCase.

```toml
dir = "logs"
name = "app"                  # active file: <dir>/<name>.<ext>
ext = "log"
level = "info"                # trace | debug | info | warn | error | fatal | off
format = "text"               # text | json
timezone = "local"            # local | utc: timestamps, rotation boundaries, archive names
console = "off"               # off | stdout | stderr
flush_interval = "100ms"      # max time in memory ("0" = write every record immediately)
buffer_size = "256KB"         # pending bytes that trigger an early write
max_buffer = "64MB"           # pending bytes at which callers wait…
overflow = "block"            # …or "drop" (counted in stats)
fsync = "rotate"              # never | rotate | always
flush_on_error = true
maintenance_interval = "1m"   # "0" disables background maintenance

[rotation]
every = "daily"               # never | minutely | hourly | daily | weekly[:sun] | 15m | 6h | 2d | W0–W6
max_size = "0"                # e.g. "100MB"
keep = 30                     # archives kept locally, 0 = all
max_age = "0"                 # e.g. "30d"

[compress]
enabled = true
after = "0s"                  # e.g. "1h": keep recent archives plain for a while
level = 6

[upload]
bucket = ""                   # set to enable
prefix = ""
object = "{prefix}{host}/{name}/{yyyy}/{mm}/{dd}/{file}"
after = "0s"
delete_local = false
# credentials = "/path/sa.json"
endpoint = "https://storage.googleapis.com"
timeout = "2m"
```

Durations accept `ms s m h d w`. Sizes accept `B KB MB GB` (powers of 1024). Unknown keys are rejected.

## Log format

**Text** (default): the timestamp, the level padded to five characters, the logger name, the message, then a TAB and logfmt fields. Newlines and tabs in messages are escaped, so one record is always one line. Nested objects are flattened (`user.id=7`), and values containing spaces, `=` or `"` are quoted.

```
2026-10-15T13:41:25.455790+01:00 ERROR api: db timeout	user=alice status=500 dur_ms=1234.5 err.stack="Error: …\n    at …"
```

**JSON**: one object per line, starting with `ts`, `level`, `logger` and `msg`, followed by your fields as given.

```json
{"ts":"2026-10-15T13:41:25.455790+01:00","level":"error","logger":"api","msg":"db timeout","user":"alice","status":500}
```

Request fields use these names: `req_id`, `method`, `path`, `status`, `dur_ms`, `ip`, `user`, `ua`, and errors use `err.{name,message,stack,code,cause}`. `stats` and the `query` shortcuts rely on these names. The readers (`tail`, `query`, `stats`) understand both formats, pino/bunyan JSON, and plain lines.

## Rotation, compression, retention and upload

- **Names**: `app.log` is the active file. Archives are named `app.2026-10-15_00-00-00.log`, using the start of their period (or the cut time for size and manual rotations), with `.1`, `.2`… on collisions and `.gz` once compressed.
- **Boundaries** are aligned to the wall clock in the configured time zone and corrected for DST. The writer rotates on time even when nothing is being logged, so yesterday's file doesn't wait for today's first record.
- **Restart**: an active file left over from an earlier period (or already over `max_size`) is archived before anything new is written.
- **Compression** writes `file.gz.tmp`, fsyncs it, renames it and then removes the original. A crash at any point leaves either the original or a complete `.gz`. Leftover `.tmp` files are cleaned on the next run.
- **Upload** waits for compression when compression is on. It uses `ifGenerationMatch=0`, so nothing is ever overwritten. A retry after a crash gets a 412, the remote size is checked, and the archive is recorded as uploaded. Uploaded archive names are recorded in `.<name>.uploaded`. Three consecutive failures stop the current run, and the next run retries. Successes and failures are logged into the stream itself (logger `rotlog`) and counted in `stats`.
- **Retention** (`keep`, `max_age`) never deletes an archive that hasn't been uploaded while a bucket is configured.
- **Where maintenance runs**: on a background thread every `maintenance_interval` and after each rotation, or via `rotlog maintain` (once, or `--watch` from cron or systemd). A per-stream lock serializes runs across processes.

## Google Cloud Storage setup

Credentials are looked up in this order:

1. `upload.access_token` / `ROTLOG_GCS_TOKEN` (a static token)
2. `upload.credentials` / `GOOGLE_APPLICATION_CREDENTIALS`: a service-account key (signed RS256 JWT exchange) or an authorized-user file
3. `~/.config/gcloud/application_default_credentials.json` (`gcloud auth application-default login`)
4. The GCE / Cloud Run / GKE metadata server

```sh
gcloud iam service-accounts create rotlog-uploader
gcloud storage buckets add-iam-policy-binding gs://my-logs \
  --member=serviceAccount:rotlog-uploader@PROJECT.iam.gserviceaccount.com \
  --role=roles/storage.objectCreator
gcloud iam service-accounts keys create sa.json --iam-account=rotlog-uploader@PROJECT.iam.gserviceaccount.com

rotlog --bucket my-logs --credentials sa.json upload --check     # verifies the token and bucket access
```

`roles/storage.objectCreator` is enough to upload. Adding `roles/storage.objectViewer` lets retries verify the size of an object that already exists. `--endpoint` points at emulators such as fake-gcs-server. Uploading is behind the `Uploader` trait in `src/upload.rs`, so another provider (S3, Azure) only needs a new implementation.

## Controlling a running writer

The process writing a stream holds an flock on `.<name>.lock`, which contains its pid. CLI commands send it requests through `.<name>.ctl`. The writer answers within about 250 ms, and this works the same whether the writer is `rotlog run`/`pipe`, a Rust program or a Bun app:

```sh
rotlog -N api level debug     # takes effect immediately; Bun sees it through a shared memory byte, with no call
rotlog -N api rotate
rotlog -N api flush
rotlog -N api status
```

`rotlog run` and `rotlog pipe` also rotate on SIGHUP, as do Bun apps with `handleSignals`.

## How it works

```
caller thread                       writer thread                     maintenance thread
─────────────                       ─────────────                     ──────────────────
encode body (msg + fields)          wake every flush_interval,        every maintenance_interval
lock                                on buffer_size, error, or         or after a rotation:
  now = realtime clock                flush/rotate request              gzip due archives
  time boundary passed? → cut       swap buffers (no copy)              upload to GCS
  write timestamp + body            write each segment; at each cut:    apply retention
  size limit hit? → cut               fsync, rename to archive,
unlock                                reopen active file
```

- Timestamps are taken while holding the lock that appends, so the bytes in a file are in timestamp order. A record stamped at 23:59:59.999999 cannot end up in the next day's file.
- Rotation points are recorded as byte offsets in the pending buffer, so a batch that crosses a boundary is split exactly.
- Callers never block on disk. If the disk falls far behind (`max_buffer`), callers wait (`overflow = "block"`, the default) or records are counted as dropped (`"drop"`). Transient write errors are retried for about 10 seconds.
- If the active file is moved or deleted externally (by logrotate, or by hand), it is reopened.

## Performance

Measured on an Apple Silicon laptop under heavy unrelated load, so expect better numbers on a quiet machine. Run `rotlog bench` and `bun run bench` in `bindings/bun` to measure your own.

| Path | Cost |
|---|---|
| Rust `log.info(msg)` | p50 ≈ 170 ns, ≈ 6 M records/s on one thread, 500–700 MB/s to disk |
| Rust with 4 fields | ≈ 250 ns |
| Bun `log.info(msg)` (JS → FFI → Rust) | ≈ 350 ns |
| Bun `log.info(msg, {4 fields})` | ≈ 750 ns (JSON) / ≈ 900 ns (text); ≈150 ns of that is `JSON.stringify` |
| Filtered-out call (below level) | 10–15 ns in Bun (no FFI call) |

The release binary is about 3 MB and the shared library about 2 MB. Most of that is TLS for uploads; without the `gcs` feature they are much smaller.

## Development

```sh
cargo test                                   # unit + engine + CLI integration tests
cargo clippy --all-targets
cd bindings/bun && bun install && bun test   # FFI binding, Bun.serve middleware, GCS upload against a mock
bunx tsc --noEmit -p bindings/bun
```

CI (`.github/workflows/ci.yml`) runs `cargo fmt --check`, clippy with `-D warnings`, the Rust tests and the Bun tests on Linux and macOS for every push and pull request.

Layout: `src/logger.rs` (writer engine), `src/format.rs` (encoding), `src/rotation.rs` (schedule and naming), `src/maintain.rs` / `src/compress.rs` / `src/gcs.rs` (archive lifecycle), `src/read.rs` (parsing and filters), `src/control.rs` (CLI ↔ writer), `src/ffi.rs` (C ABI), `src/main.rs` + `src/cli/` (CLI), `bindings/bun/` (TypeScript API).


## Releasing

Releases are published by `.github/workflows/release.yml` when a `v*` tag is pushed. It runs the tests, builds four platforms, publishes `rotlog-<platform>` and `rotlog` to npm and the crate to crates.io, and creates a GitHub release with the binaries. Versions that are already published are skipped, so a failed run can simply be re-run.

One-time setup, as GitHub repository secrets (Settings → Secrets and variables → Actions):

- `NPM_TOKEN`: an npm granular access token with read and write access to packages. For the very first publish, the token must be allowed to create new packages.
- `CARGO_REGISTRY_TOKEN`: a crates.io API token with the `publish-new` and `publish-update` scopes.

For each release:

1. Bump `version` in `Cargo.toml` and `bindings/bun/package.json` (they must match), then run `cargo build` to update `Cargo.lock`.
2. Commit, then tag and push:
   ```sh
   git tag v0.1.1 && git push origin main v0.1.1
   ```

To check the build matrix without publishing anything, run the workflow manually from the Actions tab. `bun scripts/pack-npm.ts --artifacts <dir> --allow-missing` in `bindings/bun` assembles the npm packages locally from your own builds.

## License

MIT, see [LICENSE](LICENSE).
