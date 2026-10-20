// Assembles the npm packages from built native artifacts.
//
//   bun scripts/pack-npm.ts --artifacts <dir> --out <dir> [--allow-missing]
//
// <artifacts>/<platform>/ must hold `rotlog` and `librotlog.{dylib,so}` for each
// platform below (as produced by .github/workflows/release.yml). Output:
//   <out>/rotlog-<platform>/   one package per platform (os/cpu restricted)
//   <out>/rotlog/              the TypeScript package, with optionalDependencies
//                              pinned to the platform packages of the same version
import { chmodSync, copyFileSync, cpSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { parseArgs } from "node:util";

const PLATFORMS: Record<string, { os: string; cpu: string; libc?: string; lib: string; label: string }> = {
  "darwin-arm64": { os: "darwin", cpu: "arm64", lib: "librotlog.dylib", label: "macOS arm64" },
  "darwin-x64": { os: "darwin", cpu: "x64", lib: "librotlog.dylib", label: "macOS x64" },
  "linux-x64-gnu": { os: "linux", cpu: "x64", libc: "glibc", lib: "librotlog.so", label: "Linux x64 (glibc)" },
  "linux-arm64-gnu": { os: "linux", cpu: "arm64", libc: "glibc", lib: "librotlog.so", label: "Linux arm64 (glibc)" },
};

const { values: args } = parseArgs({
  options: {
    artifacts: { type: "string" },
    out: { type: "string", default: "dist/npm" },
    "allow-missing": { type: "boolean", default: false },
  },
});
if (!args.artifacts) throw new Error("--artifacts <dir> is required");

const pkgDir = resolve(import.meta.dir, "..");
const repoRoot = resolve(pkgDir, "..", "..");
const artifacts = resolve(args.artifacts);
const out = resolve(args.out!);

const main = JSON.parse(readFileSync(join(pkgDir, "package.json"), "utf8"));
const version: string = main.version;
const cargoVersion = readFileSync(join(repoRoot, "Cargo.toml"), "utf8").match(/^version\s*=\s*"([^"]+)"/m)?.[1];
if (cargoVersion !== version) {
  throw new Error(`version mismatch: bindings/bun/package.json ${version} vs Cargo.toml ${cargoVersion}`);
}

rmSync(out, { recursive: true, force: true });
mkdirSync(out, { recursive: true });
const license = join(repoRoot, "LICENSE");
const common = {
  version,
  license: main.license,
  author: main.author,
  repository: main.repository,
  homepage: main.homepage,
  bugs: main.bugs,
};

const optionalDependencies: Record<string, string> = {};
for (const [key, p] of Object.entries(PLATFORMS)) {
  const src = join(artifacts, key);
  const files = ["rotlog", p.lib];
  const missing = files.filter((f) => !existsSync(join(src, f)));
  if (missing.length) {
    if (args["allow-missing"]) {
      console.warn(`skip ${key}: missing ${missing.join(", ")}`);
      continue;
    }
    throw new Error(`${key}: missing ${missing.map((f) => join(src, f)).join(", ")}`);
  }
  const name = `rotlog-${key}`;
  const dir = join(out, name);
  mkdirSync(dir, { recursive: true });
  for (const f of files) copyFileSync(join(src, f), join(dir, f));
  chmodSync(join(dir, "rotlog"), 0o755);
  copyFileSync(license, join(dir, "LICENSE"));
  writeFileSync(
    join(dir, "package.json"),
    JSON.stringify(
      {
        name,
        ...common,
        description: `rotlog native library and CLI for ${p.label}. Installed automatically by the rotlog package.`,
        os: [p.os],
        cpu: [p.cpu],
        ...(p.libc ? { libc: [p.libc] } : {}),
        files: [...files, "LICENSE"],
        preferUnplugged: true,
      },
      null,
      2,
    ) + "\n",
  );
  writeFileSync(
    join(dir, "README.md"),
    `# ${name}\n\nPrebuilt \`librotlog\` and \`rotlog\` CLI for ${p.label}.\n\n` +
      `Don't install this directly: install [\`rotlog\`](https://www.npmjs.com/package/rotlog), which picks the right platform package.\n`,
  );
  optionalDependencies[name] = version;
  console.log(`${name}@${version}`);
}
if (Object.keys(optionalDependencies).length === 0) throw new Error("no platform artifacts found");

const mainOut = join(out, "rotlog");
mkdirSync(mainOut, { recursive: true });
for (const d of ["src", "bin"]) cpSync(join(pkgDir, d), join(mainOut, d), { recursive: true });
chmodSync(join(mainOut, "bin", "rotlog.ts"), 0o755);
copyFileSync(join(pkgDir, "README.md"), join(mainOut, "README.md"));
copyFileSync(license, join(mainOut, "LICENSE"));
const { devDependencies: _dev, scripts: _scripts, ...published } = main;
writeFileSync(
  join(mainOut, "package.json"),
  JSON.stringify({ ...published, files: ["src", "bin", "README.md", "LICENSE"], optionalDependencies }, null, 2) + "\n",
);
console.log(`rotlog@${version} -> ${out}`);
