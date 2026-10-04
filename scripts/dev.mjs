#!/usr/bin/env node
/**
 * One command to bring up a development stack:
 *
 *   node scripts/dev.mjs [--runtime-only|--no-web|--no-server]
 *
 *   1. agentos-server   (Rust runtime: HTTP/WS gateway + gRPC)
 *   2. @agentos/server (Node control server: proxy, WS fan-out, orchestration)
 *   3. @agentos/web    (Vite dev server)
 *
 * Every child inherits stdio, so logs interleave in one terminal. Ctrl-C stops the whole tree.
 */

import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";

const args = new Set(process.argv.slice(2));
const wantRuntime = !args.has("--no-runtime");
const wantServer = !args.has("--no-server");
const wantWeb = !args.has("--no-web");
const runtimeOnly = args.has("--runtime-only");

const isWindows = process.platform === "win32";
const runtimeBin = path.resolve(
  "target",
  "debug",
  isWindows ? "agentos-server.exe" : "agentos-server",
);

const children = [];

function run(name, command, commandArgs, options = {}) {
  console.log("[" + name + "] " + command + " " + commandArgs.join(" "));
  const child = spawn(command, commandArgs, {
    stdio: "inherit",
    shell: isWindows,
    env: { ...process.env, ...options.env },
  });
  child.on("exit", (code) => {
    if (code !== 0 && code !== null) console.error("[" + name + "] exited with code " + code);
  });
  children.push({ name, child });
  return child;
}

function shutdown() {
  for (const { name, child } of children) {
    console.log("[dev] stopping " + name);
    if (!child.killed) child.kill();
  }
  process.exit(0);
}

process.on("SIGINT", shutdown);
process.on("SIGTERM", shutdown);

if (wantRuntime) {
  const usePrebuilt = existsSync(runtimeBin);
  if (usePrebuilt) {
    run("runtime", runtimeBin, []);
  } else {
    console.log("[dev] " + runtimeBin + " not found, building and running through cargo");
    run("runtime", "cargo", ["run", "-p", "agentos-server"]);
  }
}

if (!runtimeOnly && wantServer) {
  run("control", "pnpm", ["--filter", "@agentos/server", "start"], {
    env: { RUNTIME_URL: process.env.RUNTIME_URL ?? "http://127.0.0.1:8788" },
  });
}

if (!runtimeOnly && wantWeb) {
  run("web", "pnpm", ["--filter", "@agentos/web", "dev"]);
}

console.log("[dev] stack starting: runtime :8788, control :8790, web :5173");