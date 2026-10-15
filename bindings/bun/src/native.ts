// Loads the Rust core (librotlog) through bun:ffi.

import { dlopen, FFIType, suffix } from "bun:ffi";
import { existsSync } from "node:fs";
import { join } from "node:path";

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

/** Where the shared library is looked up, in order. */
export function libraryCandidates(explicit?: string): string[] {
  const here = import.meta.dir;
  return [
    explicit,
    process.env.ROTLOG_LIB,
    join(here, "..", "native", fileName),
    join(here, "..", "..", "..", "target", "release", fileName),
    join(here, "..", "..", "..", "target", "debug", fileName),
  ].filter((p): p is string => !!p);
}

let loaded: { lib: Native; path: string } | undefined;

export function native(explicit?: string): Native {
  if (loaded) return loaded.lib;
  const candidates = libraryCandidates(explicit);
  const path = candidates.find((p) => existsSync(p));
  if (!path) {
    throw new Error(
      `rotlog: native library ${fileName} not found. Build it with \`bun run build:native\` ` +
        `(or \`cargo build --release\`) or set ROTLOG_LIB. Looked in:\n  ${candidates.join("\n  ")}`,
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
