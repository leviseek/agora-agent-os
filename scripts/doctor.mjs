// Is the thing you are looking at the thing that was fixed?
//
// Answers that from the running runtime and the working tree, so nobody has to take a claim on
// trust. Usage: node scripts/doctor.mjs [--runtime URL] [--web URL] [--repo PATH]
//
// Two Windows-specific details are deliberate: the core http client instead of fetch, and
// process.exitCode instead of process.exit(). Either one otherwise aborts the process with a
// libuv assertion when the output is piped, which would destroy the verdict and its exit code.
import { readdirSync, statSync } from "node:fs";
import path from "node:path";
import http from "node:http";
import https from "node:https";

const args = process.argv.slice(2);
const flag = (name, fallback) => {
  const at = args.indexOf("--" + name);
  return at >= 0 && args[at + 1] ? args[at + 1] : fallback;
};
const repo = path.resolve(flag("repo", process.cwd()));
const runtime = flag("runtime", process.env.AGENTOS_HTTP_TARGET ?? "http://127.0.0.1:8788");
const web = flag("web", "http://localhost:5173");

// Everything this console relies on. A runtime missing one of these misbehaves in a way that
// looks like a console bug.
const REQUIRED = [
  "session.durable-recovery",
  "session.rebuild-without-directory",
  "session.closed-is-readable",
  "events.delta-streaming",
  "events.unique-sequence",
  "models.placeholder-is-fallback",
  "conversation.placeholder-filtered",
  "session.model-choice",
  "approvals.list",
];

const problems = [];
const notes = [];
const say = (line) => console.log(line);
const stamp = (ms) => (ms ? new Date(ms).toLocaleString() : "unknown");

function request(url) {
  return new Promise((resolve, reject) => {
    const client = url.startsWith("https:") ? https : http;
    const call = client.get(url, { timeout: 4000, headers: { connection: "close" } }, (response) => {
      let body = "";
      response.setEncoding("utf8");
      response.on("data", (chunk) => { body += chunk; });
      response.on("end", () => resolve({ status: response.statusCode ?? 0, body }));
    });
    call.on("timeout", () => call.destroy(new Error("timed out after 4s")));
    call.on("error", reject);
  });
}

async function getJson(url) {
  const response = await request(url);
  if (response.status < 200 || response.status >= 300) throw new Error("HTTP " + response.status);
  return JSON.parse(response.body);
}

function newestSourceMs(root) {
  let newest = 0;
  const walk = (dir) => {
    let entries;
    try { entries = readdirSync(dir, { withFileTypes: true }); } catch { return; }
    for (const entry of entries) {
      if (entry.name === "target" || entry.name === "node_modules" || entry.name.startsWith(".")) continue;
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) { walk(full); continue; }
      if (!/\.(rs|toml|proto)$/.test(entry.name)) continue;
      try { newest = Math.max(newest, statSync(full).mtimeMs); } catch { /* unreadable, skip */ }
    }
  };
  walk(root);
  return newest;
}

// --- 1. the runtime -------------------------------------------------------------------------
let meta = null;
try {
  meta = await getJson(runtime + "/v1/meta");
  say("runtime      : " + runtime + "   up " + Math.round((meta.uptime_ms ?? 0) / 1000) + "s, " + meta.sessions + " sessions");
  const build = meta.build ?? null;
  say("build        : " + (build ? build.id : "not reported - this runtime predates build reporting"));
  say("binary built : " + stamp(build?.built_at));
  say("features     : " + (build?.features?.length ? build.features.length + " reported" : "not reported"));
  if (!build) {
    problems.push("this runtime does not report its build at all: it is older than every fix here - rebuild it");
  } else {
    const present = new Set(build.features ?? []);
    const missing = REQUIRED.filter((name) => !present.has(name));
    if (missing.length > 0) problems.push("this runtime is missing: " + missing.join(", "));
    const newest = Math.max(newestSourceMs(path.join(repo, "crates")), newestSourceMs(path.join(repo, "proto")));
    if (build.built_at && newest > build.built_at) {
      problems.push("sources changed " + Math.round((newest - build.built_at) / 1000) + "s after this binary was built - it is running old code");
    }
  }
} catch (error) {
  say("runtime      : NOT REACHABLE at " + runtime + " (" + error.message + ")");
  problems.push("the runtime is not answering: start the stack with `pnpm dev` in " + repo);
}

if (meta) {
  try {
    const models = await getJson(runtime + "/v1/models");
    const configured = models.configured ?? [];
    const real = configured.filter((p) => p.configured && p.kind !== "mock" && p.kind !== "local");
    say("providers    : " + configured.map((p) => p.name + (p.configured ? "" : " (no key)")).join(", "));
    say("will answer  : " + (real.length > 0 ? real[0].name + " (a real model)" : "the built-in placeholder - no provider has a key"));
    if (real.length === 0) notes.push("set a provider key (for example DEEPSEEK_API_KEY) for real answers instead of placeholder prose");
  } catch (error) { notes.push("could not read /v1/models: " + error.message); }

  try {
    const sessions = await getJson(runtime + "/v1/sessions");
    for (const session of (sessions.sessions ?? []).slice(0, 6)) {
      let status = "?";
      try {
        const response = await request(runtime + "/v1/sessions/" + session.id + "/transcript?limit=5");
        status = String(response.status);
        if (response.status < 200 || response.status >= 300) {
          problems.push("session " + session.id + " (" + session.state + ") cannot be read: HTTP " + response.status);
        }
      } catch (error) { status = "err"; }
      say("session      : " + session.id.slice(0, 20) + "...  state=" + session.state + "  transcript=" + status);
    }
  } catch (error) { notes.push("could not list sessions: " + error.message); }
}

// --- 2. the console the browser will load ----------------------------------------------------
try {
  const response = await request(web + "/src/views/ChatView.tsx");
  const fresh = response.status === 200 && response.body.includes("liveRunVisible");
  say("console      : " + web + "   live-region code present: " + fresh);
  if (response.status !== 200) problems.push("the console dev server did not answer at " + web + ": restart `pnpm dev`");
  else if (!fresh) problems.push("the dev server is serving an older console - restart `pnpm dev`, then hard-reload the page");
} catch (error) {
  say("console      : not reachable at " + web + " (" + error.message + ")");
  problems.push("the console dev server is not answering: start `pnpm dev`");
}

// --- 3. the verdict ---------------------------------------------------------------------------
say("");
if (problems.length === 0) {
  say("VERDICT: the runtime, its build and the console all match what was fixed.");
} else {
  say("VERDICT: " + problems.length + " problem(s):");
  for (const problem of problems) say("  - " + problem);
}
for (const note of notes) say("note: " + note);
process.exitCode = problems.length === 0 ? 0 : 1;