// Capture the browser-side error behind a blank page: enable Runtime/Log BEFORE clicking.
const endpoint = process.argv[2] ?? 'http://127.0.0.1:9338';
const pageUrl = process.argv[3] ?? 'http://localhost:5173/';
const runtimeUrl = process.argv[4] ?? 'http://127.0.0.1:8788';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Never attach to a browser-internal page (edge://, devtools://): it cannot be navigated, which
// silently turns the whole check into a no-op.
const targets = await (await fetch(endpoint + '/json/list')).json();
let target = targets.find((t) => t.type === 'page' && /^https?:/.test(t.url));
if (!target) {
  const created = await fetch(endpoint + '/json/new?' + encodeURIComponent(pageUrl), { method: 'PUT' });
  target = await created.json();
}
if (!target || !target.webSocketDebuggerUrl) {
  console.error('no usable page target; got: ' + JSON.stringify(targets.map((t) => t.url)));
  process.exit(1);
}
console.log('target: ' + target.url);

const ws = new WebSocket(target.webSocketDebuggerUrl);
let nextId = 0;
const pending = new Map();
const findings = [];
ws.addEventListener('message', (event) => {
  const msg = JSON.parse(event.data);
  if (msg.id !== undefined && pending.has(msg.id)) { pending.get(msg.id)(msg); pending.delete(msg.id); return; }
  if (msg.method === 'Runtime.exceptionThrown') {
    const d = msg.params.exceptionDetails;
    findings.push('EXCEPTION: ' + (d.exception?.description ?? d.text));
  }
  if (msg.method === 'Runtime.consoleAPICalled' && (msg.params.type === 'error' || msg.params.type === 'warning')) {
    findings.push(msg.params.type.toUpperCase() + ': ' + msg.params.args.map((a) => a.description ?? a.value ?? a.type).join(' ').slice(0, 300));
  }
  if (msg.method === 'Log.entryAdded' && msg.params.entry.level === 'error') {
    findings.push('LOG: ' + msg.params.entry.text.slice(0, 300));
  }
});
const send = (method, params = {}) => new Promise((resolve) => {
  const id = ++nextId;
  pending.set(id, resolve);
  ws.send(JSON.stringify({ id, method, params }));
});
await new Promise((resolve) => ws.addEventListener('open', resolve, { once: true }));
await send('Runtime.enable');
await send('Log.enable');
await send('Page.enable');

const evaluate = async (expression) => {
  const response = await send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true });
  if (response.result?.exceptionDetails) {
    return '<threw: ' + (response.result.exceptionDetails.exception?.description ?? '?') + '>';
  }
  return response.result?.result?.value;
};

await send('Page.navigate', { url: pageUrl });
// A cold browser has to fetch every module through the dev server: 3.5s was enough on a warm
// profile and not on a fresh one, which made the check silently test an empty page.
await sleep(9000);
console.log('after first load: ' + (await evaluate("document.getElementById('root') ? document.getElementById('root').innerHTML.length : 'no #root'")));
await evaluate("localStorage.setItem('agentos.locale','zh');localStorage.setItem('agentos.theme','dark');localStorage.setItem('agentos.baseUrl','" + runtimeUrl + "');'seeded'");
await send('Page.navigate', { url: pageUrl });
await sleep(3500);

console.log('connect: ' + (await evaluate("[...document.querySelectorAll('button')].find(b=>b.textContent.trim()==='连接')?.click(); 'clicked'")));
await sleep(3500);
console.log('root html length after connect: ' + (await evaluate("document.getElementById('root').innerHTML.length")));

findings.length = 0;
const labels = await evaluate("JSON.stringify([...document.querySelectorAll('.nav-item')].map(b=>b.textContent.trim().slice(0,12)))");
console.log('nav items: ' + labels);
console.log('click settings: ' + (await evaluate("[...document.querySelectorAll('.nav-item')].find(b=>b.textContent.includes('设置'))?.click(); 'clicked'")));
await sleep(4000);
console.log('root html length after settings: ' + (await evaluate("document.getElementById('root').innerHTML.length")));
console.log('--- browser findings ---');
console.log(findings.length === 0 ? '(none)' : findings.join('\n'));
process.exit(0);
