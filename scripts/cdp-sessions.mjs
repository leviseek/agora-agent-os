// Drive the sessions view: search, then rename, and report what the DOM shows at each step.
const endpoint = process.argv[2] ?? 'http://127.0.0.1:9346';
const pageUrl = process.argv[3] ?? 'http://localhost:5179/';
const runtimeUrl = process.argv[4] ?? 'http://127.0.0.1:8831';
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
const findings = [];
ws.addEventListener('message', (event) => {
  const msg = JSON.parse(event.data);
  if (msg.id !== undefined && pending.has(msg.id)) { pending.get(msg.id)(msg); pending.delete(msg.id); return; }
  if (msg.method === 'Runtime.exceptionThrown') {
    findings.push('EXCEPTION: ' + (msg.params.exceptionDetails.exception?.description ?? '').slice(0, 200));
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
const evaluate = async (expression) => {
  const response = await send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true });
  if (response.result?.exceptionDetails) return '<threw: ' + (response.result.exceptionDetails.exception?.description ?? '?').slice(0, 160) + '>';
  return response.result?.result?.value;
};
// React ignores a plain .value assignment, so go through the native setter.
const setInput = (selector, value) =>
  evaluate("(()=>{const el=document.querySelector('" + selector + "');if(!el)return 'no input';const proto=el.tagName==='TEXTAREA'?window.HTMLTextAreaElement.prototype:window.HTMLInputElement.prototype;Object.getOwnPropertyDescriptor(proto,'value').set.call(el,'" + value + "');el.dispatchEvent(new Event('input',{bubbles:true}));return 'set'})()");
const clickByText = (selector, text) =>
  evaluate("(()=>{const el=[...document.querySelectorAll('" + selector + "')].find(e=>e.textContent.trim().includes('" + text + "'));if(!el)return 'not found: " + text + "';el.click();return 'clicked'})()");
// Only the sessions table: the detail panel below has its own runs table.
const rows = () =>
  evaluate("JSON.stringify([...document.querySelectorAll('tbody tr')].map(r=>r.innerText.split(String.fromCharCode(10))[0]).filter(t=>t.includes('ses_')).map(t=>t.slice(0,24)))");

await send('Page.navigate', { url: pageUrl });
await sleep(9000);
await evaluate("localStorage.setItem('agentos.locale','zh');localStorage.setItem('agentos.theme','dark');localStorage.setItem('agentos.baseUrl','" + runtimeUrl + "');'seeded'");
await send('Page.navigate', { url: pageUrl });
await sleep(8000);
console.log('connect : ' + (await clickByText('button.btn', '连接')));
await sleep(3000);
console.log('nav     : ' + (await clickByText('.nav-item', '会话')));
await sleep(2500);
console.log('rows    : ' + (await rows()));

await setInput("input[type=search]", 'second');
await sleep(2000);
console.log('search  : ' + (await rows()));

await setInput("input[type=search]", '');
await sleep(2000);
console.log('cleared : ' + (await rows()));

// pick the first row, then rename through the inline editor
console.log('select  : ' + (await evaluate("(()=>{const row=document.querySelector('tbody tr');if(!row)return 'no row';row.click();return 'clicked'})()")));
await sleep(2500);
console.log('rename  : ' + (await clickByText('button.btn', '改名')));
await sleep(800);
console.log('typed   : ' + (await setInput(".title-input", 'renamed by probe')));
await sleep(500);
console.log('save    : ' + (await clickByText('button.btn', '保存标题')));
await sleep(3000);
console.log('title   : ' + (await evaluate("(()=>{const p=[...document.querySelectorAll('p')].find(e=>e.querySelector('strong'));return p?p.innerText.split(String.fromCharCode(10)).join(' | '):'no detail'})()")));
console.log('rows now: ' + (await rows()));
console.log('--- exceptions ---');
for (const finding of findings) console.log(finding);
if (findings.length === 0) console.log('(none)');
process.exit(0);