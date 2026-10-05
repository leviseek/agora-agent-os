// Verify the desktop shell's folder picker is wired, without clicking it.
//
//   node scripts/cdp-desktop-check.mjs <cdp-endpoint> [screenshot-path]
//
// The shell is a WebView2 window, so it is reachable over CDP the same way a browser is - but the
// dialog the button opens is the operating system's, modal to the desktop, and nothing here can or
// should drive it. So this checks the two things a machine can: the bridge exists, and the button is
// rendered. Picking a folder is a human's click.
const endpoint = process.argv[2] ?? 'http://127.0.0.1:9353';
const screenshotPath = process.argv[3];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const targets = await (await fetch(endpoint + '/json/list')).json();
const target = targets.find((t) => t.type === 'page');
if (!target) {
  console.error('no page target at ' + endpoint + '; is the shell running with WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=... ?');
  process.exit(2);
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
    findings.push('EXCEPTION: ' + (msg.params.exceptionDetails.exception?.description ?? '').slice(0, 200));
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
    return '<threw: ' + (response.result.exceptionDetails.exception?.description ?? '?').slice(0, 200) + '>';
  }
  return response.result?.result?.value;
};
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

console.log('[page] ' + (await evaluate('window.location.href')));
console.log('[desktop bridge] typeof window.__TAURI__ = ' + (await evaluate('typeof window.__TAURI__')));
console.log(
  '[picker command] typeof invoke = ' + (await evaluate('typeof (window.__TAURI__?.core?.invoke)')),
);

// The console follows the browser language; the shell's WebView2 picks up the OS one in a Chinese
// locale, so switch to English before looking for a control by name.
await clickByText('button', 'English');
await sleep(300);
console.log('[connect] ' + (await clickByText('button', 'Connect')));
await sleep(1200);
console.log('[nav workspaces] ' + (await clickByText('button', 'Workspaces')));
await sleep(800);

const buttons = await evaluate(
  "(()=>[...document.querySelectorAll('button')].map(b=>b.textContent.trim()).filter(t=>t.length>0&&t.length<40))()",
);
console.log('[buttons] ' + JSON.stringify(buttons));
const hasExplorer = Array.isArray(buttons) && buttons.includes('Explorer...');
console.log('[explorer button rendered] ' + hasExplorer);

if (screenshotPath !== undefined && screenshotPath.length > 0) {
  const shot = await send('Page.captureScreenshot', { format: 'png' });
  const data = shot.result?.data;
  if (typeof data === 'string') {
    const { writeFile } = await import('node:fs/promises');
    await writeFile(screenshotPath, Buffer.from(data, 'base64'));
    console.log('[screenshot] ' + screenshotPath);
  }
}

console.log('[findings] ' + (findings.length === 0 ? 'none' : findings.join('; ')));
const ok = hasExplorer && findings.length === 0;
console.log(ok ? 'RESULT ok' : 'RESULT failed');
ws.close();
process.exit(ok ? 0 : 1);
