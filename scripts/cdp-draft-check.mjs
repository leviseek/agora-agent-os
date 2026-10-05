// Does a draft survive a tab switch?
//
// The bug this proves fixed: App renders one view at a time, so a form's unsent state lived in a
// component that unmounts the moment another tab is clicked. Type, switch, come back - the text was
// gone. This drives a real browser through exactly that sequence and reports what came back.
//
// Usage: node scripts/cdp-draft-check.mjs [cdp-endpoint] [console-url] [runtime-url]
const endpoint = process.argv[2] ?? "http://127.0.0.1:9346";
const pageUrl = process.argv[3] ?? "http://localhost:5173/";
const runtimeUrl = process.argv[4] ?? "http://127.0.0.1:8788";
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

const targets = await (await fetch(endpoint + "/json/list")).json();
const target = targets.find((t) => t.type === "page");
if (!target) {
  console.error("no page target at " + endpoint + ": start the browser with --remote-debugging-port");
  process.exit(2);
}
const ws = new WebSocket(target.webSocketDebuggerUrl);
let nextId = 0;
const pending = new Map();
ws.addEventListener("message", (event) => {
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
await new Promise((resolve) => ws.addEventListener("open", resolve, { once: true }));
await send("Runtime.enable");
await send("Page.enable");

const evaluate = async (expression) => {
  const response = await send("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true });
  if (response.result?.exceptionDetails) {
    return "THREW: " + (response.result.exceptionDetails.exception?.description ?? "?").slice(0, 200);
  }
  return response.result?.result?.value;
};

/** Open a view through the sidebar and report whether it actually opened. */
const openTab = async (label) => {
  const clicked = await evaluate(`(() => {
    const items = [...document.querySelectorAll('.nav-item')];
    const hit = items.find((el) => el.textContent.trim().startsWith(${JSON.stringify(label)}));
    if (!hit) return 'no tab ' + ${JSON.stringify(label)} + ' among ' + items.map((i) => i.textContent.trim()).join('/');
    hit.click();
    return 'ok';
  })()`);
  await sleep(1200);
  return clicked;
};
/** What the page is showing, for a failure that has to be diagnosable. */
const pagePeek = () =>
  evaluate("(() => (document.querySelector('main')?.innerText ?? document.body.innerText).replace(/\\s+/g,' ').slice(0, 160))()");
/** Type into a React-controlled field the way a person does, so onChange fires. */
const typeInto = (selector, text) =>
  evaluate(`(() => {
    const el = document.querySelector(${JSON.stringify(selector)});
    if (!el) return 'not found: ' + ${JSON.stringify(selector)};
    const proto = el.tagName === 'TEXTAREA' ? window.HTMLTextAreaElement.prototype : window.HTMLInputElement.prototype;
    Object.getOwnPropertyDescriptor(proto, 'value').set.call(el, ${JSON.stringify(text)});
    el.dispatchEvent(new Event('input', { bubbles: true }));
    return 'typed';
  })()`);
const valueOf = (selector) =>
  evaluate(`(() => { const el = document.querySelector(${JSON.stringify(selector)}); return el ? el.value : 'not found'; })()`);

await send("Page.navigate", { url: pageUrl });
await sleep(3000);
await evaluate(
  "localStorage.setItem('agentos.locale','zh');localStorage.setItem('agentos.baseUrl'," +
    JSON.stringify(runtimeUrl) +
    ");localStorage.setItem('agentos.token','');'seeded'",
);
await send("Page.navigate", { url: pageUrl });
await sleep(3000);
// The console boots on the connection view: connect before any other tab has content.
// The sidebar entry labelled "连接" only navigates; the button that connects carries .btn.
const connected = await evaluate(`(() => {
  const buttons = [...document.querySelectorAll('button.btn')];
  const onConnectionView = buttons.filter((b) => /连接|Connect/.test(b.textContent));
  const hit = onConnectionView.find((b) => !b.classList.contains('nav-item')) ?? onConnectionView[0];
  if (!hit) return 'no connect button among ' + buttons.map((b) => b.textContent.trim()).join('/');
  hit.click();
  return 'clicked ' + JSON.stringify(hit.textContent.trim());
})()`);
await sleep(2500);

const results = [];
const check = (name, ok, detail) => {
  results.push({ name, ok, detail });
  console.log((ok ? "PASS " : "FAIL ") + name + (detail ? "  (" + detail + ")" : ""));
};

// 1. Sessions: the new-session title field.
console.log("connect: " + connected);
console.log("open 会话: " + (await openTab("会话")));
const TITLE = "草稿不许丢";
console.log("type  : " + (await typeInto("input[type=text]", TITLE)));
await sleep(300);
const titleBefore = await valueOf("input[type=text]");
console.log("open 对话: " + (await openTab("对话")));
console.log("back 会话: " + (await openTab("会话")));
const titleAfter = await valueOf("input[type=text]");
check(
  "sessions title survives a tab switch",
  titleBefore === TITLE && titleAfter === TITLE,
  "before=" + JSON.stringify(titleBefore) + " after=" + JSON.stringify(titleAfter),
);

// Select a session so the chat view has a composer; selecting it opens the chat view.
const picked = await evaluate(`(() => {
  const rows = [...document.querySelectorAll('table tbody tr')];
  if (rows.length === 0) return 'no sessions in the table';
  const open = [...rows[0].querySelectorAll('button')].find((b) => /打开|Open/.test(b.textContent));
  if (!open) return 'no open button';
  open.click();
  return 'opened a session';
})()`);
await sleep(2000);
console.log("session: " + picked + " | " + (await pagePeek()));

// 2. The chat goal box.
const GOAL = "这段草稿必须活着";
console.log("type  : " + (await typeInto("textarea", GOAL)));
await sleep(300);
const goalBefore = await valueOf("textarea");
console.log("open 会话: " + (await openTab("会话")));
console.log("back 对话: " + (await openTab("对话")));
const goalAfter = await valueOf("textarea");
check(
  "chat goal survives a tab switch",
  goalBefore === GOAL && goalAfter === GOAL,
  "before=" + JSON.stringify(goalBefore) + " after=" + JSON.stringify(goalAfter),
);

// 3. Events: the search box.
console.log("open 事件: " + (await openTab("事件")) + " | " + (await pagePeek()));
// The events search is a plain text input in a filter bar, not type=search.
const searched = await evaluate(`(() => {
  const input = [...document.querySelectorAll('input[type=text], input[type=search]')].find(
    (el) => /搜索|Search/.test(el.placeholder ?? '') || /搜索|Search/.test(el.closest('label')?.textContent ?? ''),
  );
  if (!input) return 'no search box';
  const proto = window.HTMLInputElement.prototype;
  Object.getOwnPropertyDescriptor(proto, 'value').set.call(input, 'run_completed');
  input.dispatchEvent(new Event('input', { bubbles: true }));
  return 'typed';
})()`);
await sleep(300);
console.log("open 对话: " + (await openTab("对话")));
console.log("back 事件: " + (await openTab("事件")));
const searchAfter = await evaluate(`(() => {
  const input = [...document.querySelectorAll('input[type=text], input[type=search]')].find(
    (el) => /搜索|Search/.test(el.placeholder ?? '') || /搜索|Search/.test(el.closest('label')?.textContent ?? ''),
  );
  return input ? input.value : 'not found';
})()`);
check("events search survives a tab switch", searchAfter === "run_completed", "value=" + JSON.stringify(searchAfter));

// 4. A reload: the safe fields come back, the goal must not.
await send("Page.navigate", { url: pageUrl });
await sleep(3500);
await evaluate(`(() => {
  const buttons = [...document.querySelectorAll('button.btn')];
  const hit = buttons.find((b) => /连接|Connect/.test(b.textContent));
  if (hit) hit.click();
  return 'reconnected';
})()`);
await sleep(2500);
const reloadedGoal = await evaluate("(() => { const b = document.querySelector('textarea'); return b ? b.value : 'no textarea'; })()");
check("the goal was NOT written to localStorage", reloadedGoal !== GOAL, "value=" + JSON.stringify(reloadedGoal));

const failed = results.filter((r) => !r.ok);
console.log("");
console.log(failed.length === 0 ? "VERDICT: every draft survived" : "VERDICT: " + failed.length + " lost");
process.exitCode = failed.length === 0 ? 0 : 1;
ws.close();