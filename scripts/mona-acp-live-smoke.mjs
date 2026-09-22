// Explicit authenticated smoke. Uses only credentials already in the child
// environment/config. Never prints environment values or raw server logs.
import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import assert from 'node:assert/strict';

const executable = process.argv[2];
assert(executable, 'Pass the exact mona-acp executable to verify');
const child = spawn(executable, [], { stdio: ['pipe', 'pipe', 'pipe'] });
const waiting = new Map();
let nextId = 0;
let liveStartup = false;
let route;
child.stderr.on('data', chunk => {
  if (chunk.toString().includes('live Jev ACP classifier enabled')) liveStartup = true;
});
const timeout = setTimeout(() => {
  child.kill('SIGTERM');
  for (const { reject } of waiting.values()) reject(new Error('ACP smoke timeout'));
}, 120_000);
createInterface({ input: child.stdout }).on('line', line => {
  let frame;
  try { frame = JSON.parse(line); } catch { return; }
  const update = frame.params?.update;
  if (update?.sessionUpdate === 'router_trace') route = update;
  const waiter = waiting.get(frame.id);
  if (waiter) {
    waiting.delete(frame.id);
    if (frame.error) waiter.reject(new Error(`ACP ${frame.error.code}: ${frame.error.message}`));
    else waiter.resolve(frame.result);
  }
});
child.on('exit', () => {
  for (const { reject } of waiting.values()) reject(new Error('ACP exited before reply'));
});
function request(method, params) {
  const id = ++nextId;
  return new Promise((resolve, reject) => {
    waiting.set(id, { resolve, reject });
    child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
  });
}
try {
  await request('initialize', { protocolVersion: 1, clientCapabilities: {} });
  const session = await request('session/new', {
    provider: process.env.MONA_SMOKE_PROVIDER || 'codex',
    cwd: process.cwd(), mcpServers: [],
  });
  const result = await request('session/prompt', {
    sessionId: session.sessionId,
    prompt: [{ type: 'text', text: 'Reply with exactly READY. This is a simple greeting check. Do not use tools or change anything.' }],
  });
  assert(liveStartup, 'Startup must select live Jev, not heuristics');
  assert(route, 'Expected a router trace notification');
  assert.match(route.trace.rationale, /live Jev typed Decisions route/);
  assert.notEqual(route.trace.trigger, 'classifier_unavailable');
  // A valid live decision may be withheld by the normal confidence/safety
  // gates. Do not weaken production safeguards merely to force a smoke swap.
  assert(route.trace.requestedModel, 'Live classification must propose a concrete model');
  assert.match(result.output, /READY/);
  assert.equal(result.stopReason, 'end_turn');
  console.log(JSON.stringify({ sessionId: session.sessionId, liveStartup, route, stopReason: result.stopReason, output: result.output }, null, 2));
} finally {
  clearTimeout(timeout);
  child.stdin.end();
  child.kill('SIGTERM');
}
