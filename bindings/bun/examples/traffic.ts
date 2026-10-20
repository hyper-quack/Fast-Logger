// Sends realistic traffic to examples/server.ts so tail / query / stats have data.
//   bun run examples/traffic.ts [requestsPerSecond=20] [seconds=60] [baseUrl=http://localhost:3000]
const [rps = "20", seconds = "60", base = "http://localhost:3000"] = process.argv.slice(2);
const users = ["alice", "bob", "carol", "dave", undefined];
const pick = <T>(xs: T[]) => xs[Math.floor(Math.random() * xs.length)]!;
const route = () => {
  const r = Math.random();
  if (r < 0.75) return `/users/${1 + Math.floor(Math.random() * 50)}`;
  if (r < 0.85) return "/slow";
  if (r < 0.95) return "/nope";
  return "/boom";
};

const counts = new Map<number, number>();
const end = Date.now() + Number(seconds) * 1000;
const interval = 1000 / Number(rps);
console.log(`sending ~${rps} req/s to ${base} for ${seconds}s (Ctrl-C to stop)`);
const timer = setInterval(async () => {
  if (Date.now() > end) {
    clearInterval(timer);
    console.log("done:", Object.fromEntries(counts));
    return;
  }
  const user = pick(users);
  try {
    const res = await fetch(base + route(), { headers: user ? { "x-user": user } : {} });
    await res.arrayBuffer();
    counts.set(res.status, (counts.get(res.status) ?? 0) + 1);
  } catch (e) {
    console.error("request failed:", (e as Error).message);
  }
}, interval);
