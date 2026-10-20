// Loads the Rust core (librotlog) through bun:ffi.

import { dlopen, FFIType, suffix } from "bun:ffi";
import { accessSync, chmodSync, constants, existsSync } from "node:fs";
import { dirname, join } from "node:path";

const symbols = {
  rl_open: { args: [FFIType.ptr, FFIType.u32], returns: FFIType.ptr },
  rl_last_error: { args: [FFIType.ptr, FFIType.u32], returns: FFIType.u32 },
  rl_log: {
    args: [FFIType.ptr, FFIType.u32, FFIType.ptr, FFIType.u32, FFIType.u32, FFIType.u32],
    returns: FFIType.i32,
  },
  rl_level_ptr: { args: [FFIType.ptr], returns: FFIType.ptr },
  rl_set_level: { args: [FFIType.ptr, FFIType.u32], returns: FFIType.void },
  rl_flush: { args: [FFIType.ptr], returns: FFIType.i32 },
  rl_rotate: { args: [FFIType.ptr], returns: FFIType.i32 },
  rl_maintain: { args: [FFIType.ptr, FFIType.u32], returns: FFIType.u32 },
  rl_stats: { args: [FFIType.ptr, FFIType.ptr, FFIType.u32], returns: FFIType.u32 },
  rl_close: { args: [FFIType.ptr], returns: FFIType.void },
  rl_version: { args: [FFIType.ptr, FFIType.u32], returns: FFIType.u32 },
} as const;

export type Native = ReturnType<typeof dlopen<typeof symbols>>["symbols"];

const fileName = process.platform === "win32" ? "rotlog.dll" : `librotlog.${suffix}`;

/** npm package holding the prebuilt library and CLI for this machine, e.g. `rotlog-darwin-arm64`. */
export const platformPackage = `rotlog-${process.platform}-${process.arch}${process.platform === "linux" ? "-gnu" : ""}`;

/** Directory of the installed platform package, if any. */
function platformDir(): string | undefined {
  try {
    return dirname(Bun.resolveSync(`${platformPackage}/package.json`, import.meta.dir));
  } catch {
    return undefined;
  }
}

/** Where a native file is looked up, in order: platform package, then a local build. */
function nativeCandidates(file: string): string[] {
  const here = import.meta.dir;
  const pkg = platformDir();
  return [
    pkg && join(pkg, file),
    join(here, "..", "native", file),
    join(here, "..", "..", "..", "target", "release", file),
    join(here, "..", "..", "..", "target", "debug", file),
  ].filter((p): p is string => !!p);
}

/** Where the shared library is looked up, in order. */
export function libraryCandidates(explicit?: string): string[] {
  return [explicit, process.env.ROTLOG_LIB, ...nativeCandidates(fileName)].filter((p): p is string => !!p);
}

/** Path of the `rotlog` CLI binary (`ROTLOG_BIN`, the platform package, or a local build). */
export function cliPath(): string {
  const exe = process.platform === "win32" ? "rotlog.exe" : "rotlog";
  const candidates = [process.env.ROTLOG_BIN, ...nativeCandidates(exe)].filter((p): p is string => !!p);
  const path = candidates.find((p) => existsSync(p));
  if (!path) {
    throw new Error(
      `rotlog: no prebuilt CLI for ${process.platform}-${process.arch} (package ${platformPackage}). ` +
        `Install it with \`cargo install rotlog\` or set ROTLOG_BIN. Looked in:\n  ${candidates.join("\n  ")}`,
    );
  }
  try {
    accessSync(path, constants.X_OK);
  } catch {
    chmodSync(path, 0o755); // registries and CI artifacts may drop the executable bit
  }
  return path;
}

let loaded: { lib: Native; path: string } | undefined;

export function native(explicit?: string): Native {
  if (loaded) return loaded.lib;
  const candidates = libraryCandidates(explicit);
  const path = candidates.find((p) => existsSync(p));
  if (!path) {
    throw new Error(
      `rotlog: native library ${fileName} not found for ${process.platform}-${process.arch}. ` +
        `Install the matching optional package (${platformPackage}), build it with \`bun run build:native\`, ` +
        `or set ROTLOG_LIB. Looked in:\n  ${candidates.join("\n  ")}`,
    );
  }
  loaded = { lib: dlopen(path, symbols).symbols, path };
  return loaded.lib;
}

export function nativePath(): string | undefined {
  return loaded?.path;
}

export function lastError(lib: Native): string {
  const len = lib.rl_last_error(null, 0);
  if (len === 0) return "unknown error";
  const buf = Buffer.alloc(len);
  lib.rl_last_error(buf, len);
  return buf.toString("utf8");
}
