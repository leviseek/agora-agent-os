// A minimal MCP server over stdio, used by the adapter tests.
// Newline-delimited JSON-RPC 2.0: one message per line, in both directions.
const tools = [
  {
    name: 'echo',
    description: 'Echo the text back',
    inputSchema: { type: 'object', required: ['text'], properties: { text: { type: 'string' } } },
  },
  {
    name: 'fail',
    description: 'Always reports an error',
    inputSchema: { type: 'object' },
  },
  {
    name: 'weird',
    description: 'Publishes a schema the runtime cannot validate',
    inputSchema: { type: ['string', 'null'] },
  },
];

const send = (message) => process.stdout.write(JSON.stringify(message) + '\n');

let buffer = '';
process.stdin.on('data', (chunk) => {
  buffer += chunk.toString('utf8');
  let index;
  while ((index = buffer.indexOf('\n')) >= 0) {
    const line = buffer.slice(0, index).trim();
    buffer = buffer.slice(index + 1);
    if (line.length > 0) handle(line);
  }
});

function handle(line) {
  let message;
  try {
    message = JSON.parse(line);
  } catch {
    return;
  }
  // Notifications carry no id and get no reply.
  if (message.id === undefined) return;
  const reply = (result) => send({ jsonrpc: '2.0', id: message.id, result });
  const fail = (code, text) => send({ jsonrpc: '2.0', id: message.id, error: { code, message: text } });

  switch (message.method) {
    case 'initialize':
      reply({
        protocolVersion: '2024-11-05',
        capabilities: { tools: {} },
        serverInfo: { name: 'fake-mcp', version: '1.0.0' },
      });
      return;
    case 'tools/list':
      reply({ tools });
      return;
    case 'tools/call': {
      const name = message.params?.name;
      const args = message.params?.arguments ?? {};
      if (name === 'echo') {
        reply({ content: [{ type: 'text', text: 'echo: ' + String(args.text ?? '') }] });
        return;
      }
      if (name === 'fail') {
        reply({ content: [{ type: 'text', text: 'deliberate failure' }], isError: true });
        return;
      }
      if (name === 'weird') {
        reply({ content: [{ type: 'text', text: 'weird ok' }] });
        return;
      }
      fail(-32602, 'unknown tool ' + String(name));
      return;
    }
    default:
      fail(-32601, 'method not found: ' + String(message.method));
  }
}
