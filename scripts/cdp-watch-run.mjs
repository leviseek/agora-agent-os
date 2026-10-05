// Watch the console with its own socket status visible.
const endpoint = process.argv[2];
const pageUrl = process.argv[3];
const runtimeUrl = process.argv[4];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const targets = await (await fetch(endpoint + '/json/list')).json();
// Any page will do: the script navigates it itself. A freshly started browser only has about:blank,
// and demanding an http(s) page first is how this probe used to die on its own.
const target =
  targets.find((t) => t.type === 'page' && /^https?:/.test(t.url)) ??
  targets.find((t) => t.type === 'page');
const ws = new WebSocket(target.webSocketDebuggerUrl);
let nextId = 0;
const pending = new Map();
ws.addEventListener('message', (event) => {
  const msg = JSON.parse(event.data);
  if (msg.id !== undefined && pending.has(msg.id)) { pending.get(msg.id)(msg); pending.delete(msg.id); }
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
  if (response.result?.exceptionDetails) return '<threw: ' + (response.result.exceptionDetails.exception?.description ?? '?').slice(0, 200) + '>';
  return response.result?.result?.value;
};
const clickByText = (selector, text) =>
  evaluate("(()=>{const el=[...document.querySelectorAll('" + selector + "')].find(e=>e.textContent.includes('" + text + "'));if(!el)return 'not found';el.click();return 'clicked'})()");

await send('Page.navigate', { url: pageUrl });
await sleep(9000);
await evaluate("localStorage.setItem('agentos.locale','zh');localStorage.setItem('agentos.baseUrl','" + runtimeUrl + "');'ok'");
await send('Page.navigate', { url: pageUrl });
await sleep(8000);
console.log('connect : ' + (await clickByText('button.btn', '连接')));
await sleep(3500);
console.log('chat nav: ' + (await clickByText('.nav-item', '对话')));
await sleep(2500);
const before = await evaluate("(()=>{const t=document.body.innerText;const m=t.match(/事件通道[^\\n]*/);return m?m[0]:'no socket line'})()");
console.log('socket  : ' + before);
console.log('runs before: ' + (await evaluate("document.querySelectorAll('.run').length")));

const watcher = [
  '(async () => {',
  '  const rt = ' + JSON.stringify(runtimeUrl) + ';',
  '  const sessions = await (await fetch(rt + "/v1/sessions")).json();',
  '  const id = sessions.sessions[0].id;',
  '  fetch(rt + "/v1/sessions/" + id + "/messages", {',
  '    method: "POST", headers: { "content-type": "application/json" },',
  '    body: JSON.stringify({ text: "what is 21*2? explain the steps please", wait: false }),',
  '  });',
  '  const out = [];',
  '  for (let i = 0; i < 40; i += 1) {',
  '    const runs = document.querySelectorAll(".run");',
  '    const badges = runs.length > 0',
  '      ? [...runs[runs.length - 1].querySelectorAll(".badge")].map((b) => b.textContent.trim()).join("/")',
  '      : "";',
  '    const live = document.querySelector(".streaming-text");',
  '    out.push({ t: i * 200, runs: runs.length, badges: badges, live: live ? live.textContent.length : -1 });',
  '    await new Promise((r) => setTimeout(r, 200));',
  '  }',
  '  const deltaText = [...document.querySelectorAll(".stream-kind")].filter((e) => e.textContent.includes("agent_delta")).length;',
  '  return JSON.stringify({ samples: out.filter((_, i) => i % 3 === 0), agentDeltaLines: deltaText });',
  '})()',
].join('\n');
console.log('watch   : ' + (await evaluate(watcher)));
process.exit(0);
