// Batch-three verification: watch an answer stream in, see an attached image, approve a parked call.
const endpoint = process.argv[2] ?? 'http://127.0.0.1:9347';
const pageUrl = process.argv[3] ?? 'http://localhost:5180/';
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
const errors = [];
ws.addEventListener('message', (event) => {
  const msg = JSON.parse(event.data);
  if (msg.id !== undefined && pending.has(msg.id)) { pending.get(msg.id)(msg); pending.delete(msg.id); return; }
  if (msg.method === 'Runtime.exceptionThrown') {
    errors.push((msg.params.exceptionDetails.exception?.description ?? '').slice(0, 200));
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
  if (response.result?.exceptionDetails) return '<threw: ' + (response.result.exceptionDetails.exception?.description ?? '?').slice(0, 160) + '>';
  return response.result?.result?.value;
};
const setInput = (selector, value) =>
  evaluate("(()=>{const el=document.querySelector('" + selector + "');if(!el)return 'no element';const proto=el.tagName==='TEXTAREA'?window.HTMLTextAreaElement.prototype:window.HTMLInputElement.prototype;Object.getOwnPropertyDescriptor(proto,'value').set.call(el," + JSON.stringify(value) + ");el.dispatchEvent(new Event('input',{bubbles:true}));return 'set'})()");
const clickByText = (selector, text) =>
  evaluate("(()=>{const el=[...document.querySelectorAll('" + selector + "')].find(e=>e.textContent.trim().includes(" + JSON.stringify(text) + "));if(!el)return 'not found';el.click();return 'clicked'})()");

await send('Page.navigate', { url: pageUrl });
await sleep(9000);
await evaluate("localStorage.setItem('agentos.locale','zh');localStorage.setItem('agentos.theme','dark');localStorage.setItem('agentos.baseUrl','" + runtimeUrl + "');'seeded'");
await send('Page.navigate', { url: pageUrl });
await sleep(8000);
console.log('connect : ' + (await clickByText('button.btn', '连接')));
await sleep(3000);
console.log('chat nav: ' + (await clickByText('.nav-item', '对话')));
await sleep(2500);

// --- a goal with an image, posted without waiting so the stream can be watched -----------------
console.log('goal    : ' + (await setInput('textarea', 'what is 6*7? please explain')));
console.log('image   : ' + (await setInput('.image-paths', 'pixel.png')));
console.log('send    : ' + (await clickByText('button.btn', '发送')));

let sawStream = '';
for (let attempt = 0; attempt < 60; attempt += 1) {
  const text = await evaluate("(()=>{const el=document.querySelector('.streaming-text');return el?el.textContent:''})()");
  if (typeof text === 'string' && text.length > sawStream.length) sawStream = text;
  if (sawStream.length > 40) break;
  await sleep(200);
}
console.log('streamed: ' + JSON.stringify(sawStream.slice(0, 90)) + ' (' + sawStream.length + ' chars captured live)');
await sleep(6000);

const attachments = await evaluate("JSON.stringify([...document.querySelectorAll('.attachment')].map(a=>a.innerText.trim()))");
console.log('attach  : ' + attachments);
console.log('img src : ' + (await evaluate("(()=>{const i=document.querySelector('.attachment img');return i?(i.naturalWidth+'x'+i.naturalHeight+' loaded='+i.complete):'none'})()")));

// --- park a call that needs approval, then approve it from the console -------------------------
const parked = evaluate(
  "fetch('" + runtimeUrl + "/v1/capabilities/filesystem-write/invoke',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({input:{path:'approved-from-console.txt',content:'approved in the console'}})}).then(r=>r.status)",
  false,
);
await sleep(1500);
console.log('nav     : ' + (await clickByText('.nav-item', '审批')));
await sleep(3000);
const rows = await evaluate("JSON.stringify([...document.querySelectorAll('tbody tr')].map(r=>r.innerText.split(String.fromCharCode(10)).slice(0,3).join(' | ')))");
console.log('pending : ' + rows);
console.log('approve : ' + (await clickByText('button.btn', '批准')));
await sleep(2500);
const after = await evaluate("JSON.stringify([...document.querySelectorAll('tbody tr')].map(r=>r.innerText.split(String.fromCharCode(10))[0]))");
console.log('after   : ' + after);
console.log('call    : HTTP ' + (await parked));
console.log('--- exceptions ---');
console.log(errors.length === 0 ? '(none)' : errors.join('\n'));
process.exit(0);
