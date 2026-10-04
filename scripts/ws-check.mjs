// Does the gateway actually push events over the socket? No browser involved.
const runtime = process.argv[2];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const sessions = await (await fetch(runtime + '/v1/sessions')).json();
const id = sessions.sessions[0].id;
console.log('session: ' + id);

const ws = new WebSocket(runtime.replace('http', 'ws') + '/v1/ws');
const frames = [];
ws.addEventListener('message', (event) => {
  let parsed = null;
  try { parsed = JSON.parse(String(event.data)); } catch { return; }
  if (parsed.type === 'hello') { frames.push('hello'); return; }
  if (parsed.type === 'event') {
    const kind = parsed.event?.kind ?? '?';
    const payload = parsed.event?.payload ?? {};
    frames.push(kind + (kind === 'agent_delta' ? ':' + String(payload.text ?? '').length : ''));
  } else {
    frames.push('<' + (parsed.type ?? '?') + '>');
  }
});
await new Promise((resolve, reject) => {
  ws.addEventListener('open', resolve, { once: true });
  ws.addEventListener('error', () => reject(new Error('socket error')), { once: true });
  setTimeout(() => reject(new Error('socket did not open')), 5000);
});
console.log('socket: connected');

await fetch(runtime + '/v1/sessions/' + id + '/messages', {
  method: 'POST',
  headers: { 'content-type': 'application/json' },
  body: JSON.stringify({ text: 'what is 21*2? explain the steps please', wait: false }),
});
await sleep(6000);
const deltas = frames.filter((f) => f.startsWith('agent_delta'));
const chars = deltas.map((d) => Number(d.split(':')[1] ?? 0)).reduce((a, b) => a + b, 0);
console.log('frames total: ' + frames.length);
console.log('first kinds : ' + frames.slice(0, 16).join(', '));
console.log('delta frames: ' + deltas.length + '  chars: ' + chars);
ws.close();
process.exit(0);
