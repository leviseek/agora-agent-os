// Does the console actually render streamed text? Polls the live region while a goal runs.
const endpoint = process.argv[2];
const pageUrl = process.argv[3];
const runtimeUrl = process.argv[4];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const targets = await (await fetch(endpoint + '/json/list')).json();
let target = targets.find((t) => t.type === 'page' && /^https?:/.test(t.url));
if (!target) {
  const created = await fetch(endpoint + '/json/new?' + encodeURIComponent(pageUrl), { method: 'PUT' });
  target = await created.json();
}
const ws = new WebSocket(target.webSocketDebuggerUrl);
let nextId = 0;
const pending = new Map();
const errors = [];
ws.addEventListener('message', (event) => {
  const msg = JSON.parse(event.data);
  if (msg.id !== undefined && pending.has(msg.id)) { pending.get(msg.id)(msg); pending.delete(msg.id); return; }
  if (msg.method === 'Runtime.exceptionThrown') {
    errors.push((msg.params.exceptionDetails.exception?.description ?? '').slice(0, 160));
  }
});
const send = (method, params = {}) => new Promise((resolve) => {
  const id = ++nextId;
  pending.set(id, resolve);
  ws.send(JSON.stringify({ id, method, params }));
});
await new Promise((resolve) => ws.addEventListener('open', resolve, { once: true }));
await send('Runtime.enable');
await send('Page.enable');
const evaluate = async (expression, awaitPromise = true) => {
  const response = await send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise });
  if (response.result?.exceptionDetails) {
    return '<threw: ' + (response.result.exceptionDetails.exception?.description ?? '?').slice(0, 200) + '>';
  }
  return response.result?.result?.value;
};
const clickByText = (selector, text) =>
  evaluate("(()=>{const el=[...document.querySelectorAll('" + selector + "')].find(e=>e.textContent.includes('" + text + "'));if(!el)return 'not found';el.click();return 'clicked'})()");

await send('Page.navigate', { url: pageUrl });
await sleep(9000);
await evaluate("localStorage.setItem('agentos.locale','zh');localStorage.setItem('agentos.theme','dark');localStorage.setItem('agentos.baseUrl','" + runtimeUrl + "');'ok'");
await send('Page.navigate', { url: pageUrl });
await sleep(8000);
console.log('connect: ' + (await clickByText('button.btn', '连接')));
await sleep(3000);
console.log('nav    : ' + (await clickByText('.nav-item', '对话')));
await sleep(2500);

const watcher = [
  '(async () => {',
  '  const rt = ' + JSON.stringify(runtimeUrl) + ';',
  '  const sessions = await (await fetch(rt + "/v1/sessions")).json();',
  '  const id = sessions.sessions[0].id;',
  '  fetch(rt + "/v1/sessions/" + id + "/messages", {',
  '    method: "POST",',
  '    headers: { "content-type": "application/json" },',
  '    body: JSON.stringify({ text: "what is 21*2? explain the steps please", wait: false }),',
  '  });',
  '  const out = [];',
  '  for (let i = 0; i < 80; i += 1) {',
  '    const live = document.querySelector(".streaming-text");',
  '    const pendingBubble = document.querySelector(".bubble-pending");',
  '    out.push({ t: i * 150, live: live ? live.textContent.length : -1, pending: pendingBubble ? 1 : 0 });',
  '    await new Promise((r) => setTimeout(r, 150));',
  '  }',
  '  const deltas = await (await fetch(rt + "/v1/events?limit=200&kinds=agent_delta")).json();',
  '  const chars = deltas.events.map((e) => e.payload.text).join("").length;',
  '  const answers = document.querySelectorAll(".bubble-agent:not(.bubble-pending)");',
  '  const last = answers.length > 0 ? answers[answers.length - 1].textContent.length : -1;',
  '  return JSON.stringify({',
  '    samples: out.filter((_, i) => i % 5 === 0),',
  '    maxLiveChars: Math.max.apply(null, out.map((s) => s.live)),',
  '    everShowedLive: out.some((s) => s.live > 0),',
  '    deltaEvents: deltas.events.length,',
  '    deltaChars: chars,',
  '    finalAnswerChars: last,',
  '  });',
  '})()',
].join('\n');
console.log('result : ' + (await evaluate(watcher)));
console.log('errors : ' + (errors.length === 0 ? '(none)' : errors.join(' | ')));
process.exit(0);
