// Does one session's streaming answer stay out of another session's transcript?
//
// The bug this pins down: the console's live preview was keyed by run alone, and the socket
// carries every session's events - so opening session A while session B was still answering showed
// B's words under A. The check proves both directions, because only one of them fails on the
// broken build: the marker must be visible while B is selected, and absent once A is.
//
// Usage: node scripts/cdp-chat-leak-check.mjs [cdp-endpoint] [console-url] [runtime-url]
const endpoint = process.argv[2] ?? 'http://127.0.0.1:9355';
const consoleUrl = process.argv[3] ?? 'http://localhost:5173/';
const runtimeUrl = process.argv[4] ?? 'http://127.0.0.1:8788';
const MARKER = 'ZEBRA-LEAK-MARKER';
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

const targets = await (await fetch(endpoint + '/json/list')).json();
const target = targets.find((t) => t.type === 'page');
if (!target) {
  console.error('no page target at ' + endpoint);
  process.exit(2);
}
const ws = new WebSocket(target.webSocketDebuggerUrl);
let nextId = 0;
const pending = new Map();
ws.addEventListener('message', (event) => {
  const message = JSON.parse(event.data);
  if (message.id !== undefined && pending.has(message.id)) {
    pending.get(message.id)(message);
    pending.delete(message.id);
  }
});
const send = (method, params = {}) =>
  new Promise((resolve) => {
    const id = ++nextId;
    pending.set(id, resolve);
    ws.send(JSON.stringify({ id, method, params }));
  });
await new Promise((resolve) => ws.addEventListener('open', resolve, { once: true }));
await send('Runtime.enable');
await send('Page.enable');
const evaluate = async (expression) => {
  const response = await send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true });
  if (response.result?.exceptionDetails) {
    return 'THREW: ' + (response.result.exceptionDetails.exception?.description ?? '?').slice(0, 200);
  }
  return response.result?.result?.value;
};

const create = async (title) => {
  const response = await fetch(runtimeUrl + '/v1/sessions', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ user_id: 'operator', title }),
  });
  const body = await response.json();
  return body.id;
};
const titleA = 'leak-check A watched';
const titleB = 'leak-check B streaming';
const sessionA = await create(titleA);
const sessionB = await create(titleB);
console.log('sessions: A=' + sessionA.slice(0, 14) + ' B=' + sessionB.slice(0, 14));

await send('Page.navigate', { url: consoleUrl });
await sleep(3000);
await evaluate("localStorage.setItem('agentos.locale','zh');localStorage.setItem('agentos.baseUrl','" + runtimeUrl + "');localStorage.setItem('agentos.token','');'seeded'");
await send('Page.navigate', { url: consoleUrl });
await sleep(3000);
const connected = await evaluate("(() => { const button = [...document.querySelectorAll('button.btn')].find((b) => /连接|Connect/.test(b.textContent)); if (!button) return 'no connect button'; button.click(); return 'connected'; })()");
await sleep(2500);
console.log('connect: ' + connected);

const openSession = async (title) => {
  const nav = await evaluate("(() => { const item = [...document.querySelectorAll('.nav-item')].find((el) => el.textContent.trim().startsWith('会话')); if (!item) return 'no sessions tab'; item.click(); return 'ok'; })()");
  await sleep(1200);
  const opened = await evaluate('(() => { const rows = [...document.querySelectorAll("table tbody tr")]; const row = rows.find((r) => r.textContent.includes(' + JSON.stringify(title) + ')); if (!row) return "row not found"; const button = [...row.querySelectorAll("button")].find((b) => /打开|Open/.test(b.textContent)); if (!button) return "no open button"; button.click(); return "opened"; })()');
  await sleep(1800);
  return nav + '/' + opened;
};
const transcriptText = () => evaluate("(() => { const el = document.querySelector('.transcript'); return el ? el.innerText : ''; })()");

console.log('open B: ' + (await openSession(titleB)));
const prompt = '请把 ' + MARKER + ' 这一串原样重复输出 60 次，每次单独一行，不要任何解释。';
const started = await evaluate('(async () => { const response = await fetch(' + JSON.stringify(runtimeUrl) + ' + ' + JSON.stringify('/v1/sessions/' + sessionB + '/messages') + ', { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ text: ' + JSON.stringify(prompt) + ', wait: false }) }); return response.status; })()');
console.log('B goal posted: ' + started);

let sawMarkerInB = false;
let bChars = 0;
for (let attempt = 0; attempt < 40; attempt += 1) {
  const text = await transcriptText();
  if (typeof text === 'string') {
    bChars = text.length;
    if (text.includes(MARKER)) {
      sawMarkerInB = true;
      break;
    }
  }
  await sleep(500);
}
console.log('marker visible while B is open: ' + sawMarkerInB + ' (B must show its own live text)');

console.log('open A: ' + (await openSession(titleA)));
let leaked = false;
let aChars = 0;
for (let attempt = 0; attempt < 16; attempt += 1) {
  const text = await transcriptText();
  if (typeof text === 'string') {
    aChars = text.length;
    if (text.includes(MARKER)) {
      leaked = true;
      break;
    }
  }
  await sleep(500);
}
console.log('marker visible while A is open: ' + leaked + ' (A must show nothing of B)');

const failures = [];
if (!sawMarkerInB) failures.push('B never showed its own streaming answer (' + bChars + ' chars), so the check proves nothing');
if (leaked) failures.push('B streaming answer appeared in A transcript (' + aChars + ' chars)');
console.log('');
console.log(failures.length === 0 ? 'VERDICT: live text stays in its own session' : 'VERDICT: ' + failures.join('; '));
process.exitCode = failures.length === 0 ? 0 : 1;
ws.close();
