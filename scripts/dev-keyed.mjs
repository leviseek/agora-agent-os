// Start the stack with the provider keys this machine already has, so a vision-capable model is
// actually usable.
//
// The problem this solves: an API key set at User scope belongs to processes started after it was
// set. A terminal (or IDE) that was already open keeps the old environment, so the runtime starts
// with no credential, the router falls back to the built-in placeholder, and asking it about a
// picture produces prose about an image nobody looked at. Reading the key here removes the guess.
//
// It never writes a key anywhere, prints one, or passes one on a command line: the value travels
// only in this process's environment, which the child inherits.
import { spawn } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const KEYS = ["DEEPSEEK_API_KEY", "OPENAI_API_KEY", "DASHSCOPE_API_KEY", "AGENTOS_LOCAL_API_KEY"];

const borrowed = [];
for (const name of KEYS) {
  if (typeof process.env[name] === "string" && process.env[name].trim().length > 0) continue;
  const escaped = name.replace(/'/g, "''");
  const probe = spawn(
    process.platform === "win32" ? "powershell.exe" : "sh",
    process.platform === "win32"
      ? ["-NoProfile", "-Command", `[Environment]::GetEnvironmentVariable('${escaped}','User')`]
      : ["-c", `printenv ${name} || true`],
    { stdio: ["ignore", "pipe", "ignore"] },
  );
  const chunks = [];
  probe.stdout.on("data", (chunk) => chunks.push(chunk));
  await new Promise((resolve) => probe.on("close", resolve));
  const value = Buffer.concat(chunks).toString("utf8").trim();
  if (value.length > 0) {
    process.env[name] = value;
    borrowed.push(name);
  }
}

if (borrowed.length > 0) {
  console.log("[dev:keyed] picked up from this machine: " + borrowed.join(", "));
} else {
  console.log("[dev:keyed] no provider key found in the environment or at User scope");
}
if (!process.env.DEEPSEEK_API_KEY && !process.env.OPENAI_API_KEY && !process.env.DASHSCOPE_API_KEY) {
  console.log(
    "[dev:keyed] warning: no hosted provider key, so answers come from the built-in placeholder and " +
      "an attached image will be refused. Set one, for example:",
  );
  console.log("  [Environment]::SetEnvironmentVariable('DEEPSEEK_API_KEY', '<key>', 'User')");
}

const child = spawn(process.execPath, [path.join(repoRoot, "scripts", "dev.mjs"), ...process.argv.slice(2)], {
  cwd: repoRoot,
  stdio: "inherit",
  env: process.env,
});
child.on("exit", (code, signal) => {
  process.exitCode = signal === null ? (code ?? 0) : 1;
});
