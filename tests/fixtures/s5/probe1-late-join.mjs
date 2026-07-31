// S5 probe: does a late-joining app-server connection receive thread events?
const URL_ = process.argv[2];
const CWD = process.argv[3];

const logs = { A: [], B: [] };
const t0 = Date.now();
const ts = () => `+${String(Date.now() - t0).padStart(6, ' ')}ms`;

function mkClient(name) {
  const ws = new WebSocket(URL_);
  const pending = new Map();
  let nextId = 1;
  const c = {
    name, ws,
    onNotify: [],
    log(dir, msg) {
      const rec = { t: Date.now() - t0, dir, msg };
      logs[name].push(rec);
      const m = msg.method || (msg.result !== undefined ? `result#${msg.id}` : msg.error ? `ERROR#${msg.id}` : '?');
      console.log(`${ts()} [${name}] ${dir} ${m}`);
    },
    send(obj) { c.log('->', obj); ws.send(JSON.stringify(obj)); },
    req(method, params) {
      const id = nextId++;
      const p = new Promise((res, rej) => pending.set(id, { res, rej }));
      c.send({ jsonrpc: '2.0', id, method, params });
      return p;
    },
    notify(method, params) { c.send({ jsonrpc: '2.0', method, params }); },
  };
  ws.addEventListener('message', (ev) => {
    let msg; try { msg = JSON.parse(ev.data); } catch { return; }
    c.log('<-', msg);
    if (msg.id !== undefined && (msg.result !== undefined || msg.error !== undefined)) {
      const p = pending.get(msg.id);
      if (p) { pending.delete(msg.id); msg.error ? p.rej(new Error(JSON.stringify(msg.error))) : p.res(msg.result); }
    } else if (msg.method) {
      for (const f of c.onNotify) f(msg);
    }
  });
  ws.addEventListener('close', () => console.log(`${ts()} [${name}] CLOSED`));
  ws.addEventListener('error', (e) => console.log(`${ts()} [${name}] WSERR ${e.message}`));
  c.ready = new Promise((res) => ws.addEventListener('open', res));
  return c;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function handshake(c) {
  await c.ready;
  await c.req('initialize', { clientInfo: { name: `s5-probe-${c.name}`, version: '0.0.1' } });
  c.notify('initialized', {});
}

// count thread-ish notifications received by a client after a mark
function marker(c) {
  const got = [];
  const f = (msg) => got.push({ t: Date.now() - t0, method: msg.method, threadId: msg.params?.threadId });
  c.onNotify.push(f);
  return { got, stop: () => { c.onNotify = c.onNotify.filter((x) => x !== f); } };
}

async function waitTurnDone(c, timeoutMs = 90000) {
  return new Promise((res) => {
    const to = setTimeout(() => { c.onNotify = c.onNotify.filter(x => x !== f); res('timeout'); }, timeoutMs);
    const f = (msg) => {
      if (/turn\/(completed|failed|aborted)/.test(msg.method || '') || /turnCompleted|turnFailed/.test(msg.method || '')) {
        clearTimeout(to); c.onNotify = c.onNotify.filter(x => x !== f); res(msg.method);
      }
    };
    c.onNotify.push(f);
  });
}

const results = {};

(async () => {
  const A = mkClient('A');
  await handshake(A);

  const started = await A.req('thread/start', { cwd: CWD });
  const threadId = started.threadId ?? started.thread?.id ?? started.thread_id;
  console.log(`${ts()} THREAD=${threadId}`);
  results.threadId = threadId;

  // ---- Client B joins late ----
  const B = mkClient('B');
  await handshake(B);
  const readRes = await B.req('thread/read', { threadId }).catch(e => ({ error: e.message }));
  results.threadReadOk = !readRes.error;

  await sleep(300);

  // ---- PHASE 1: A issues the turn ----
  console.log('\n===== PHASE 1: turn from A =====');
  const mA1 = marker(A), mB1 = marker(B);
  await A.req('turn/start', { threadId, input: [{ type: 'text', text: 'reply with the single word alpha' }] });
  const d1 = await waitTurnDone(A);
  console.log(`${ts()} phase1 done via ${d1}`);
  await sleep(1500);
  mA1.stop(); mB1.stop();
  results.phase1 = { A: mA1.got, B: mB1.got };
  console.log(`PHASE1: A got ${mA1.got.length} notifications, B got ${mB1.got.length}`);

  // ---- PHASE 2: B issues the turn (B never subscribed) ----
  console.log('\n===== PHASE 2: turn from B =====');
  const mA2 = marker(A), mB2 = marker(B);
  let p2err = null;
  await B.req('turn/start', { threadId, input: [{ type: 'text', text: 'reply with the single word beta' }] }).catch(e => { p2err = e.message; });
  results.phase2Error = p2err;
  const d2 = await Promise.race([waitTurnDone(A), waitTurnDone(B)]);
  console.log(`${ts()} phase2 done via ${d2}`);
  await sleep(1500);
  mA2.stop(); mB2.stop();
  results.phase2 = { A: mA2.got, B: mB2.got };
  console.log(`PHASE2: A got ${mA2.got.length}, B got ${mB2.got.length}`);

  // ---- PHASE 3: B calls thread/resume on the live thread, then A issues a turn ----
  console.log('\n===== PHASE 3: B thread/resume then turn from A =====');
  let resumeRes = null, resumeErr = null;
  try { resumeRes = await B.req('thread/resume', { threadId }); }
  catch (e) { resumeErr = e.message; }
  results.resume = { ok: !resumeErr, err: resumeErr };
  await sleep(500);

  const mA3 = marker(A), mB3 = marker(B);
  await A.req('turn/start', { threadId, input: [{ type: 'text', text: 'reply with the single word gamma' }] });
  const d3 = await Promise.race([waitTurnDone(A), waitTurnDone(B)]);
  console.log(`${ts()} phase3 done via ${d3}`);
  await sleep(1500);
  mA3.stop(); mB3.stop();
  results.phase3 = { A: mA3.got, B: mB3.got };
  console.log(`PHASE3: A got ${mA3.got.length}, B got ${mB3.got.length}`);

  // ---- PHASE 4: unknown method probe (method enumeration) ----
  const probe = await A.req('thread/subscribe', { threadId }).then(r => ({ ok: r })).catch(e => ({ err: e.message }));
  results.subscribeProbe = probe;

  const fs = await import('node:fs');
  fs.writeFileSync(process.argv[4] || 'result.json', JSON.stringify({ results, logs }, null, 2));
  console.log('\nDONE');
  A.ws.close(); B.ws.close();
  process.exit(0);
})().catch(e => { console.error('FATAL', e); process.exit(1); });
