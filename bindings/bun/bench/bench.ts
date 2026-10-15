// bun run bench/bench.ts  — per-call cost of the JS -> Rust path.
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createLogger, withContext } from "../src/index";

const N = 300_000;

function run(label: string, fn: (i: number) => void) {
  for (let i = 0; i < 10_000; i++) fn(i); // warm up the JIT
  const t0 = Bun.nanoseconds();
  for (let i = 0; i < N; i++) fn(i);
  const ns = (Bun.nanoseconds() - t0) / N;
  console.log(`${label.padEnd(34)} ${ns.toFixed(0).padStart(5)} ns/call  ${(1e3 / ns).toFixed(2)} M/s`);
}

for (const format of ["text", "json"] as const) {
  const dir = mkdtempSync(join(tmpdir(), "rotlog-bench-"));
  const log = createLogger({ dir, name: "bench", format, compress: false, maintenanceInterval: 0 });
  console.log(`format=${format}`);
  run("info(msg)", () => log.info("user logged in"));
  run("info(msg, 4 fields)", (i) => log.info("request handled", { method: "GET", path: "/api/users/42", status: 200, i }));
  const child = log.child({ service: "billing", region: "eu" });
  run("child.info(msg, 2 fields)", (i) => child.info("charge created", { amount: 1299, i }));
  withContext({ req_id: "3f2b8c1e", user: "u42" }, () => {
    run("info in request context", (i) => log.info("loading profile", { i }));
  });
  run("debug (filtered out)", () => log.debug("not written", { a: 1 }));
  const t0 = Bun.nanoseconds();
  log.flush();
  const s = log.stats();
  console.log(`flush ${((Bun.nanoseconds() - t0) / 1e6).toFixed(1)} ms, ${s.records.toLocaleString()} records, ${(s.bytes / 1e6).toFixed(0)} MB, dropped ${s.dropped}\n`);
  log.close();
  rmSync(dir, { recursive: true, force: true });
}
