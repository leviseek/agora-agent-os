// A stand-in for a hosted model, with the two behaviours that matter here:
//   * it refuses JSON mode when the prompt never mentions JSON, exactly as DeepSeek does, and
//   * it streams its answer a few characters at a time, so "did the text arrive gradually?" is
//     a question with an answer.
// Usage: node scripts/fake-model.mjs [--port 8899] [--delay 25] [--chunk 3]
import http from "node:http";

const args = process.argv.slice(2);
const flag = (name, fallback) => {
  const at = args.indexOf("--" + name);
  return at >= 0 && args[at + 1] ? Number(args[at + 1]) : fallback;
};
const port = flag("port", 8899);
const delay = flag("delay", 25);
const chunkSize = flag("chunk", 3);

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const seen = { calls: 0, refused: 0, streamed: 0 };

function readBody(request) {
  return new Promise((resolve) => {
    let body = "";
    request.on("data", (chunk) => { body += chunk; });
    request.on("end", () => resolve(body));
  });
}

const server = http.createServer(async (request, response) => {
  if (request.url === "/stats") {
    response.writeHead(200, { "content-type": "application/json" });
    response.end(JSON.stringify(seen));
    return;
  }
  if (!request.url.endsWith("/chat/completions")) {
    response.writeHead(404).end("not found");
    return;
  }
  const body = JSON.parse((await readBody(request)) || "{}");
  seen.calls += 1;
  const prompt = JSON.stringify(body.messages ?? []);
  const wantsJson = body.response_format?.type === "json_object";
  // DeepSeeks documented rule: json_object needs the word json somewhere in the prompt.
  if (wantsJson && !/json/i.test(prompt)) {
    seen.refused += 1;
    response.writeHead(400, { "content-type": "application/json" });
    response.end(JSON.stringify({ error: { message: "Prompt must contain the word 'json' to use json_object", type: "invalid_request_error" } }));
    return;
  }

  const isPlanning = /Respond with JSON only/.test(prompt);
  const goal = /"content":"([^"]*)"/.exec(prompt)?.[1] ?? "the goal";
  const answer = isPlanning
    ? JSON.stringify({
        goal,
        reasoning: "one step: answer directly",
        steps: [{ id: "respond-1", description: "Answer directly.", kind: "respond", input: {}, depends_on: [] }],
      })
    : "Here is the answer, written out a few characters at a time so it arrives gradually: " +
      "the goal was understood, nothing needed a capability, and this text is being streamed " +
      "token by token rather than delivered in one lump at the end.";

  const wantsStream = body.stream === true;
  if (!wantsStream) {
    response.writeHead(200, { "content-type": "application/json" });
    response.end(JSON.stringify({
      id: "cmpl-fake",
      object: "chat.completion",
      model: body.model ?? "fake",
      choices: [{ index: 0, message: { role: "assistant", content: answer }, finish_reason: "stop" }],
      usage: { prompt_tokens: 100, completion_tokens: 50, total_tokens: 150 },
    }));
    return;
  }

  seen.streamed += 1;
  response.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache", connection: "keep-alive" });
  for (let at = 0; at < answer.length; at += chunkSize) {
    const piece = answer.slice(at, at + chunkSize);
    response.write("data: " + JSON.stringify({
      id: "cmpl-fake",
      object: "chat.completion.chunk",
      model: body.model ?? "fake",
      choices: [{ index: 0, delta: { content: piece }, finish_reason: null }],
    }) + "\n\n");
    await sleep(delay);
  }
  response.write("data: " + JSON.stringify({
    id: "cmpl-fake",
    object: "chat.completion.chunk",
    model: body.model ?? "fake",
    choices: [{ index: 0, delta: {}, finish_reason: "stop" }],
    usage: { prompt_tokens: 100, completion_tokens: 50, total_tokens: 150 },
  }) + "\n\n");
  response.write("data: [DONE]\n\n");
  response.end();
});

server.listen(port, "127.0.0.1", () => console.log("fake model listening on http://127.0.0.1:" + port));