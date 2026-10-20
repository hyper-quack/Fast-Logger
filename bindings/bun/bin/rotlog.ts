#!/usr/bin/env bun
// `rotlog` CLI entry for npm installs: runs the prebuilt native binary.
import { constants } from "node:os";
import { cliPath } from "../src/native";

const child = Bun.spawn([cliPath(), ...process.argv.slice(2)], { stdio: ["inherit", "inherit", "inherit"] });

// A terminal Ctrl-C already reaches the child (same process group); don't send it twice.
process.on("SIGINT", () => {});
for (const sig of ["SIGTERM", "SIGHUP"] as const) process.on(sig, () => child.kill(sig));

const code = await child.exited;
const sig = child.signalCode;
const signo = typeof sig === "number" ? sig : sig ? constants.signals[sig] : 0;
process.exit(sig ? 128 + signo : code);
