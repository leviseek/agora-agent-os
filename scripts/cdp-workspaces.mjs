// Drive the workspace flow in a real browser: connect, create a workspace, create a session inside
// it, and report what the DOM shows at each step.
//
//   node scripts/cdp-workspaces.mjs <cdp-endpoint> <console-url>
//
// Exists because the workspace model is only "done" when a person can see it: the console has to
// group sessions by workspace, create into the one that is selected, and create a workspace without
// anyone explaining that "my default workspace" is a thing. Prints the visible text it found and any
// browser-side exception or console error, so a failure is a fact rather than an opinion.
const endpoint = process.argv[2] ?? 'http://127.0.0.1:9346';
const pageUrl = process.argv[3] ?? 'http://localhost:5174/';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const targets = await (await fetch(endpoint + '/json/list')).json();
let target = targets.find((t) => t.type === 'page');
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
  if (msg.id !== undefined && pending.has(msg.id)) {
    pending.get(msg.id)(msg);
    pending.delete(msg.id);
    return;
  }
  if (msg.method === 'Runtime.exceptionThrown') {
    findings.push(
      'EXCEPTION: ' + (msg.params.exceptionDetails.exception?.description ?? '').slice(0, 240),
    );
  }
  if (msg.method === 'Runtime.consoleAPICalled' && msg.params.type === 'error') {
    findings.push(
      'CONSOLE.ERROR: ' +
        (msg.params.args ?? [])
          .map((a) => a.value ?? a.description ?? '')
          .join(' ')
          .slice(0, 240),
    );
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
  const response = await send('Runtime.evaluate', {
    expression,
    returnByValue: true,
    awaitPromise: true,
  });
  if (response.result?.exceptionDetails) {
    return '<threw: ' + (response.result.exceptionDetails.exception?.description ?? '?').slice(0, 200) + '>';
  }
  return response.result?.result?.value;
};
// React ignores a plain .value assignment, so go through the native setter.
const setInput = (selector, value) =>
  evaluate(
    "(()=>{const el=document.querySelector('" +
      selector +
      "');if(!el)return 'no input';const proto=el.tagName==='TEXTAREA'?window.HTMLTextAreaElement.prototype:window.HTMLInputElement.prototype;Object.getOwnPropertyDescriptor(proto,'value').set.call(el,'" +
      value +
      "');el.dispatchEvent(new Event('input',{bubbles:true}));return 'set'})()",
  );
// Exact match first, then a substring: the nav button "Connection" must not swallow a click meant
// for the "Connect" action, and the two differ only by a suffix.
const clickByText = (selector, text) =>
  evaluate(
    "(()=>{const all=[...document.querySelectorAll('" +
      selector +
      "')];const el=all.find(e=>e.textContent.trim()==='" +
      text +
      "')??all.find(e=>e.textContent.trim().includes('" +
      text +
      "'));if(!el)return 'not found: " +
      text +
      "';el.click();return 'clicked'})()",
  );
const lines = async () =>
  (await evaluate('document.body.innerText'))
    .split('\n')
    .map((line) => line.trim())
    .filter((line) => line.length > 0);

const step = async (label, action) => {
  const outcome = await action();
  console.log(`[${label}] ${outcome}`);
  return outcome;
};

/** Poll until `check` is true or the budget runs out. */
const waitFor = async (check, timeoutMs, intervalMs = 250) => {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    if (await check()) return true;
    if (Date.now() >= deadline) return false;
    await sleep(intervalMs);
  }
};

const workspaceName = 'cdp workspace ' + Date.now().toString().slice(-5);

await send('Page.navigate', { url: pageUrl });
await sleep(1800);

// The console follows the browser's language, so a headless run in a Chinese locale sees Chinese
// labels. Switch to English first: the script then looks for one spelling of each control.
await step('language en', () => clickByText('button', 'English'));
await sleep(400);

await step('connect', () => clickByText('button', 'Connect'));
await sleep(1500);

await step('nav workspaces', () => clickByText('button', 'Workspaces'));
await sleep(600);
await step('create form', () => setInput('input[placeholder="e.g. sprite rework"]', workspaceName));
await sleep(200);
await step('create workspace', () => clickByText('button', 'Create workspace'));
await sleep(1500);

const workspacesPage = await lines();
console.log('--- workspaces page ---');
console.log(workspacesPage.slice(30, 70).join(' | '));

await step('nav sessions', () => clickByText('button', 'Sessions'));
await sleep(800);
await step('session title', () => setInput('input[placeholder="investigate flaky deploy"]', 'cdp session'));
await sleep(200);
await step('create session', () => clickByText('button', 'Create session'));
await sleep(2000);

// Creating a session jumps to Chat, so come back to the list to see where it landed. Then wait for
// the row rather than for a fixed delay: the list is refreshed over the socket and a sleep is a
// guess about how fast that is.
await step('nav sessions again', () => clickByText('button', 'Sessions'));
const landed = await waitFor(
  () => lines().then((all) => all.some((line) => line.includes(workspaceName))),
  8000,
);
console.log('[session row in its workspace] ' + (landed ? 'found' : 'not found'));

const sessionsPage = await lines();
console.log('--- sessions page (after creating one) ---');
console.log(sessionsPage.slice(30, 70).join(' | '));

await step('back to workspaces', () => clickByText('button', 'Workspaces'));
await sleep(900);
const backAgain = await lines();
console.log('--- workspaces page again ---');
console.log(backAgain.slice(30, 70).join(' | '));

const ok =
  workspacesPage.some((line) => line.includes(workspaceName)) &&
  landed &&
  findings.length === 0;
console.log('--- findings ---');
console.log(findings.length === 0 ? 'none' : findings.join('\n'));
console.log(ok ? 'RESULT ok' : 'RESULT failed');
ws.close();
process.exit(ok ? 0 : 1);
