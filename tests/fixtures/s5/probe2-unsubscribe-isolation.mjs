// S5 probe 2: unsubscribe isolation + history integrity after resume-attach.
// Two brand-new connections attach to an EXISTING live thread via thread/resume.
const URL_ = process.argv[2];
const THREAD = process.argv[3];
const OUT = process.argv[4];

const logs = { C: [], D: [] };
const t0 = Date.now();
const ts = () => `+${String(Date.now() - t0).padStart(6, ' ')}ms`;

function mkClient(name) {
  const ws = new WebSocket(URL_);
  const pending = new Map(); let nextId = 1;
  const c = { name, ws, onNotify: [],
    log(dir, msg) { logs[name].push({ t: Date.now() - t0, dir, msg });
      const m = msg.method || (msg.result !== undefined ? `result#${msg.id}` : `ERROR#${msg.id}`);
      console.log(`${ts()} [${name}] ${dir} ${m}`); },
    send(o) { c.log('->', o); ws.send(JSON.stringify(o)); },
    req(method, params) { const id = nextId++;
      const p = new Promise((res, rej) => pending.set(id, { res, rej }));
      c.send({ jsonrpc: '2.0', id, method, params }); return p; },
    notify(method, params) { c.send({ jsonrpc: '2.0', method, params }); } };
  ws.addEventListener('message', (ev) => { let m; try { m = JSON.parse(ev.data); } catch { return; }
    c.log('<-', m);
    if (m.id !== undefined && (m.result !== undefined || m.error !== undefined)) {
      const p = pending.get(m.id); if (p) { pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result); }
    } else if (m.method) for (const f of c.onNotify) f(m); });
  c.ready = new Promise((r) => ws.addEventListener('open', r));
  return c;
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function handshake(c) { await c.ready;
  await c.req('initialize', { clientInfo: { name: `s5-probe2-${c.name}`, version: '0.0.1' } });
  c.notify('initialized', {}); }
function marker(c) { const got = []; const f = (m) => got.push({ t: Date.now() - t0, method: m.method });
  c.onNotify.push(f); return { got, stop: () => { c.onNotify = c.onNotify.filter((x) => x !== f); } }; }
function waitDone(c, ms = 90000) { return new Promise((res) => {
  const to = setTimeout(() => { c.onNotify = c.onNotify.filter(x => x !== f); res('timeout'); }, ms);
  const f = (m) => { if (/turn\/(completed|failed|aborted)/.test(m.method || '')) { clearTimeout(to); c.onNotify = c.onNotify.filter(x => x !== f); res(m.method); } };
  c.onNotify.push(f); }); }

const results = {};
(async () => {
  const C = mkClient('C'); const D = mkClient('D');
  await handshake(C); await handshake(D);

  // Both attach to the pre-existing, already-loaded thread.
  results.resumeC = await C.req('thread/resume', { threadId: THREAD }).then(() => 'ok').catch(e => e.message);
  results.resumeD = await D.req('thread/resume', { threadId: THREAD }).then(() => 'ok').catch(e => e.message);
  await sleep(300);

  // History integrity: does the thread still hold all prior turns after repeated resume?
  const read = await C.req('thread/read', { threadId: THREAD, includeTurns: true }).catch(e => ({ error: e.message }));
  results.turnCount = read?.thread?.turns?.length ?? read?.turns?.length ?? null;
  results.readKeys = Object.keys(read || {});

  // D detaches.
  results.unsubD = await D.req('thread/unsubscribe', { threadId: THREAD }).then(() => 'ok').catch(e => e.message);
  await sleep(300);

  const mC = marker(C), mD = marker(D);
  await C.req('turn/start', { threadId: THREAD, input: [{ type: 'text', text: 'reply with the single word delta' }] });
  await waitDone(C);
  await sleep(1500);
  mC.stop(); mD.stop();
  results.afterUnsub = { C: mC.got.map(x => x.method), D: mD.got.map(x => x.method) };
  console.log(`AFTER-UNSUB: C got ${mC.got.length}, D got ${mD.got.length}`);

  const fs = await import('node:fs');
  fs.writeFileSync(OUT, JSON.stringify({ results, logs }, null, 2));
  console.log('DONE'); C.ws.close(); D.ws.close(); process.exit(0);
})().catch(e => { console.error('FATAL', e); process.exit(1); });
