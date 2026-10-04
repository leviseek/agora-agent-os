// Drive the chat flow in a real browser and report what the transcript shows over time.
const endpoint = process.argv[2] ?? 'http://127.0.0.1:9343';
const pageUrl = process.argv[3] ?? 'http://localhost:5175/';
const runtimeUrl = process.argv[4] ?? 'http://127.0.0.1:8801';
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
const clickByText = (selector, text) =>
  evaluate("(()=>{const el=[...document.querySelectorAll('" + selector + "')].find(e=>e.textContent.includes('" + text + "'));if(!el)return 'not found: " + text + "';el.click();return 'clicked " + text + "'})()");

await send('Page.navigate', { url: pageUrl });
await sleep(9000);
await evaluate("localStorage.setItem('agentos.locale','zh');localStorage.setItem('agentos.theme','dark');localStorage.setItem('agentos.baseUrl','" + runtimeUrl + "');'seeded'");
await send('Page.navigate', { url: pageUrl });
await sleep(8000);
console.log('connect : ' + (await clickByText('button.btn', '连接')));
await sleep(3500);
console.log('sessions: ' + (await clickByText('.nav-item', '会话')));
await sleep(2500);
console.log('new     : ' + (await clickByText('button', '新建会话')));
await sleep(3000);
console.log('chat    : ' + (await clickByText('.nav-item', '对话')));
await sleep(2500);

// Type into the goal box the React way, then leave "wait" unchecked and submit.
const typed = await evaluate(`(()=>{
  const box = document.querySelector('textarea') ?? document.querySelector('input[type=text]');
  if (!box) return 'no goal box';
  const proto = box.tagName === 'TEXTAREA' ? window.HTMLTextAreaElement.prototype : window.HTMLInputElement.prototype;
  Object.getOwnPropertyDescriptor(proto, 'value').set.call(box, 'what is 8*8?');
  box.dispatchEvent(new Event('input', { bubbles: true }));
  const waitBox = [...document.querySelectorAll('input[type=checkbox]')].find(c => c.closest('label')?.textContent.includes('等待'));
  return 'typed; wait checkbox checked=' + (waitBox ? waitBox.checked : 'not found');
})()`);
console.log('input   : ' + typed);
await sleep(600);
console.log('nav-chat: ' + (await clickByText('.nav-item', '对话')));
console.log('submit  : ' + (await clickByText('button.btn', '发送')));
for (let i = 1; i <= 8; i += 1) {
  await sleep(1500);
  const state = await evaluate("(()=>{const box=document.querySelector('.transcript');const bubbles=[...document.querySelectorAll('.bubble')].map(b=>b.className.replace('bubble ','')+': '+b.innerText.split(String.fromCharCode(10)).join(' | ').slice(0,60));const atBottom=box?Math.abs(box.scrollHeight-box.scrollTop-box.clientHeight)<4:null;const ack=[...document.querySelectorAll('.muted.small')].map(e=>e.innerText).find(x=>x.includes('目标已接受'))??'';return JSON.stringify({atBottom,bubbles,ack})})()");
  console.log('t+' + (i * 1.5) + 's: ' + state);
}
console.log('--- exceptions ---');
for (const finding of findings) console.log(finding);
if (findings.length === 0) console.log('(none)');
process.exit(0);
