// S5 probe 3: can a late joiner attach via thread/resume WHILE a turn is in flight,
// and does doing so disturb the in-flight turn or the originating connection?
const URL_ = process.argv[2]; const CWD = process.argv[3]; const OUT = process.argv[4];
const logs = { A: [], B: [] }; const t0 = Date.now();
const ts = () => `+${String(Date.now() - t0).padStart(6, ' ')}ms`;
function mkClient(name) {
  const ws = new WebSocket(URL_); const pending = new Map(); let nextId = 1;
  const c = { name, ws, onNotify: [],
    log(dir, m) { logs[name].push({ t: Date.now() - t0, dir, msg: m });
      console.log(`${ts()} [${name}] ${dir} ${m.method || (m.result !== undefined ? `result#${m.id}` : `ERROR#${m.id}`)}`); },
    send(o) { c.log('->', o); ws.send(JSON.stringify(o)); },
    req(method, params) { const id = nextId++; const p = new Promise((res, rej) => pending.set(id, { res, rej }));
      c.send({ jsonrpc: '2.0', id, method, params }); return p; },
    notify(method, params) { c.send({ jsonrpc: '2.0', method, params }); } };
  ws.addEventListener('message', (ev) => { let m; try { m = JSON.parse(ev.data); } catch { return; } c.log('<-', m);
    if (m.id !== undefined && (m.result !== undefined || m.error !== undefined)) { const p = pending.get(m.id);
      if (p) { pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result); } }
    else if (m.method) for (const f of c.onNotify) f(m); });
  c.ready = new Promise((r) => ws.addEventListener('open', r)); return c;
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function handshake(c) { await c.ready; await c.req('initialize', { clientInfo: { name: `s5-p3-${c.name}`, version: '0.0.1' } }); c.notify('initialized', {}); }
function marker(c) { const got = []; const f = (m) => got.push({ t: Date.now() - t0, method: m.method }); c.onNotify.push(f); return { got, stop: () => { c.onNotify = c.onNotify.filter(x => x !== f); } }; }
function waitDone(c, ms = 120000) { return new Promise((res) => { const to = setTimeout(() => { c.onNotify = c.onNotify.filter(x => x !== f); res('timeout'); }, ms);
  const f = (m) => { if (/turn\/(completed|failed|aborted)/.test(m.method || '')) { clearTimeout(to); c.onNotify = c.onNotify.filter(x => x !== f); res(m.method); } }; c.onNotify.push(f); }); }
const results = {};
(async () => {
  const A = mkClient('A'); await handshake(A);
  const st = await A.req('thread/start', { cwd: CWD });
  const threadId = st.threadId ?? st.thread?.id; results.threadId = threadId;
  const mA = marker(A);
  // long-ish streaming turn
  A.req('turn/start', { threadId, input: [{ type: 'text', text: 'Print the numbers 1 through 40, one per line, nothing else.' }] });
  await sleep(2500); // let the turn get underway
  const B = mkClient('B'); await handshake(B);
  const mB = marker(B);
  const tAttach = Date.now() - t0;
  results.attachAtMs = tAttach;
  results.resumeMidTurn = await B.req('thread/resume', { threadId }).then(() => 'ok').catch(e => e.message);
  console.log(`${ts()} B resumed mid-turn: ${results.resumeMidTurn}`);
  const done = await waitDone(A);
  results.turnOutcome = done;
  await sleep(1500); mA.stop(); mB.stop();
  results.A = mA.got.map(x => `${x.t}:${x.method}`);
  results.B = mB.got.map(x => `${x.t}:${x.method}`);
  console.log(`MIDTURN: A ${mA.got.length} events, B ${mB.got.length} events (B attached at ${tAttach}ms)`);
  const fs = await import('node:fs'); fs.writeFileSync(OUT, JSON.stringify({ results, logs }, null, 2));
  console.log('DONE'); A.ws.close(); B.ws.close(); process.exit(0);
})().catch(e => { console.error('FATAL', e); process.exit(1); });
