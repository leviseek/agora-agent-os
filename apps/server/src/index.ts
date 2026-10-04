/**
 * Control server: a thin, typed facade in front of the Agent OS runtime.
 *
 *   GET  /healthz            control server + runtime health
 *   GET  /api/meta           runtime metadata
 *   GET  /api/sessions       session list
 *   GET  /api/capabilities   capability list
 *   POST /api/orchestrate    {goal, strategy} -> orchestrated multi-session run
 *   ALL  /api/runtime/*      transparent proxy to the runtime (one origin for the browser)
 *   WS   /ws                 fan-out of the runtime event stream to every browser client
 *
 * The proxy exists so the browser only ever talks to one origin. It deliberately does not cache
 * or reinterpret runtime payloads.
 */

import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import { WebSocketServer, WebSocket } from "ws";
import { AgentOsError, RuntimeClient } from "./client.ts";
import { orchestrate, planWithRuntime } from "./orchestrator.ts";

const PORT = Number(process.env.PORT ?? 8790);
const RUNTIME_URL = process.env.RUNTIME_URL ?? "http://127.0.0.1:8788";
const TOKEN = process.env.AGENTOS_AUTH_TOKEN;

const runtime = new RuntimeClient(RUNTIME_URL, TOKEN);

function json(res: ServerResponse, status: number, body: unknown): void {
  const text = JSON.stringify(body);
  res.writeHead(status, { "content-type": "application/json", "content-length": Buffer.byteLength(text) });
  res.end(text);
}

async function readBody(req: IncomingMessage): Promise<unknown> {
  const chunks: Buffer[] = [];
  for await (const chunk of req) chunks.push(chunk as Buffer);
  const text = Buffer.concat(chunks).toString("utf8");
  if (text.length === 0) return {};
  try {
    return JSON.parse(text);
  } catch {
    throw new Error("request body must be JSON");
  }
}

async function proxy(req: IncomingMessage, res: ServerResponse, path: string): Promise<void> {
  const headers: Record<string, string> = { "content-type": req.headers["content-type"] ?? "application/json" };
  if (TOKEN) headers.authorization = "Bearer " + TOKEN;
  const body =
    req.method === "GET" || req.method === "HEAD" ? undefined : await readBody(req).catch(() => undefined);
  try {
    const upstream = await fetch(RUNTIME_URL + path, {
      method: req.method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    const text = await upstream.text();
    res.writeHead(upstream.status, { "content-type": upstream.headers.get("content-type") ?? "application/json" });
    res.end(text);
  } catch (error) {
    json(res, 502, { error: { code: "runtime_unreachable", message: String(error), retryable: true } });
  }
}

async function handle(req: IncomingMessage, res: ServerResponse): Promise<void> {
  const url = new URL(req.url ?? "/", "http://localhost");
  const path = url.pathname;

  if (path === "/healthz") {
    try {
      const health = await runtime.health();
      json(res, 200, { control_server: "ok", runtime: health, runtime_url: RUNTIME_URL });
    } catch (error) {
      json(res, 503, { control_server: "ok", runtime: "unreachable", detail: String(error) });
    }
    return;
  }

  if (path === "/api/meta") {
    json(res, 200, await runtime.meta());
    return;
  }

  if (path === "/api/sessions") {
    json(res, 200, { sessions: await runtime.listSessions() });
    return;
  }

  if (path === "/api/capabilities") {
    json(res, 200, { capabilities: await runtime.listCapabilities() });
    return;
  }

  if (path === "/api/orchestrate" && req.method === "POST") {
    const body = (await readBody(req)) as { goal?: string; strategy?: "single" | "fan-out"; plan?: boolean };
    const goal = (body.goal ?? "").trim();
    if (goal.length === 0) {
      json(res, 400, { error: { code: "invalid_input", message: "goal is required", retryable: false } });
      return;
    }
    if (body.plan === true) {
      json(res, 200, { goal, sub_goals: await planWithRuntime(runtime, goal) });
      return;
    }
    json(res, 200, await orchestrate(runtime, goal, { strategy: body.strategy ?? "fan-out" }));
    return;
  }

  if (path.startsWith("/api/runtime/")) {
    // Accept both /api/runtime/capabilities and /api/runtime/v1/capabilities; never double /v1.
    const rest = path.slice("/api/runtime".length);
    const target = rest.startsWith("/v1/") || rest === "/v1" ? rest : "/v1" + rest;
    await proxy(req, res, target + url.search);
    return;
  }

  json(res, 404, { error: { code: "not_found", message: "no route for " + path, retryable: false } });
}

const server = createServer((req, res) => {
  void handle(req, res).catch((error) => {
    if (error instanceof AgentOsError) {
      json(res, error.status, { error: { code: error.code, message: error.message, retryable: error.retryable } });
    } else {
      json(res, 500, { error: { code: "internal", message: String(error), retryable: false } });
    }
  });
});

// --- WebSocket fan-out ------------------------------------------------------------------------
// One upstream socket to the runtime, N downstream sockets to browsers: clients come and go
// without multiplying subscriptions inside the runtime.

const wss = new WebSocketServer({ server, path: "/ws" });
const clients = new Set<WebSocket>();

let upstream: WebSocket | null = null;
let reconnectDelay = 500;

function connectUpstream(): void {
  upstream = new WebSocket(runtime.wsUrl());
  upstream.on("open", () => {
    reconnectDelay = 500;
    console.log("[ws] connected to runtime at " + runtime.wsUrl());
  });
  upstream.on("message", (data) => {
    const text = data.toString();
    for (const client of clients) {
      if (client.readyState === WebSocket.OPEN) client.send(text);
    }
  });
  upstream.on("close", () => {
    console.log("[ws] runtime socket closed, reconnecting in " + reconnectDelay + "ms");
    setTimeout(connectUpstream, reconnectDelay);
    reconnectDelay = Math.min(reconnectDelay * 2, 15000);
  });
  upstream.on("error", (error) => {
    console.warn("[ws] upstream error: " + String(error));
  });
}

wss.on("connection", (socket) => {
  clients.add(socket);
  socket.send(JSON.stringify({ type: "hello", source: "control-server", runtime_url: RUNTIME_URL }));
  socket.on("close", () => clients.delete(socket));
  socket.on("message", (data) => {
    // Forward commands verbatim; the runtime owns the protocol.
    if (upstream && upstream.readyState === WebSocket.OPEN) upstream.send(data.toString());
  });
});

connectUpstream();

server.listen(PORT, () => {
  console.log("agora-agent-os control server listening on http://127.0.0.1:" + PORT);
  console.log("  runtime : " + RUNTIME_URL);
  console.log("  ws      : ws://127.0.0.1:" + PORT + "/ws");
});