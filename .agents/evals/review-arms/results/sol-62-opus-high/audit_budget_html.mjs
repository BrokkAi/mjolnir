// Drive the budget-map HTML in headless Chrome and compare real hover tooltips with brute force.
// Usage: node audit_budget_html.mjs ARTIFACT_DIR SCREENSHOT_DIR CHROME_HEADLESS_SHELL
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';

const [artifacts, shots, chrome] = process.argv.slice(2);
const stem = 'review-opus-high-budget';
const payload = JSON.parse(fs.readFileSync(path.join(artifacts, `${stem}-data.json`), 'utf8'));
fs.mkdirSync(shots, { recursive: true });
let failures = 0;
const check = (ok, msg) => { console.log((ok ? 'ok   ' : 'FAIL ') + msg); if (!ok) failures++; };

const profile = fs.mkdtempSync(path.join(shots, 'profile-'));
const proc = spawn(chrome, ['--headless', '--no-sandbox', '--disable-gpu', '--remote-debugging-port=0',
  '--user-data-dir=' + profile, '--window-size=1400,1500', 'about:blank'],
  { stdio: ['ignore', 'ignore', 'pipe'] });
const port = await new Promise((resolve, reject) => {
  let buf = '';
  proc.stderr.on('data', d => { buf += d; const m = buf.match(/ws:\/\/127\.0\.0\.1:(\d+)\//); if (m) resolve(m[1]); });
  proc.on('exit', c => reject(new Error('chrome exited ' + c + ' ' + buf)));
});
const targets = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
const ws = new WebSocket(targets.find(t => t.type === 'page').webSocketDebuggerUrl);
await new Promise(r => ws.addEventListener('open', r));
let nextId = 1; const pending = new Map(); const consoleErrors = [];
ws.addEventListener('message', ev => {
  const m = JSON.parse(ev.data);
  if (m.id && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id); }
  else if (m.method === 'Runtime.exceptionThrown') consoleErrors.push(JSON.stringify(m.params.exceptionDetails));
  else if (m.method === 'Runtime.consoleAPICalled' && m.params.type === 'error') consoleErrors.push(JSON.stringify(m.params.args));
  else if (m.method === 'Network.requestWillBeSent' && !m.params.request.url.startsWith('file:')) consoleErrors.push('network: ' + m.params.request.url);
});
const send = (method, params = {}) => new Promise(res => { const id = nextId++; pending.set(id, res); ws.send(JSON.stringify({ id, method, params })); });
const evalJs = async expr => { const r = await send('Runtime.evaluate', { expression: expr, returnByValue: true }); if (r.result.exceptionDetails) throw new Error(JSON.stringify(r.result.exceptionDetails)); return r.result.result.value; };
await send('Page.enable'); await send('Runtime.enable'); await send('Network.enable');
await send('Emulation.setDeviceMetricsOverride', { width: 1400, height: 1500, deviceScaleFactor: 1, mobile: false });
await send('Page.navigate', { url: 'file://' + path.resolve(artifacts, `${stem}.html`) });
await new Promise(r => setTimeout(r, 1500));

const brute = (P, T, C) => {
  const feasible = P.options.filter(o => o.t <= T && o.c <= C);
  if (!feasible.length) return null;
  const best = feasible.reduce((m, o) => Math.max(m, o.f), -Infinity);
  return { best, labels: feasible.filter(o => o.f === best).map(o => o.label).sort() };
};
const toClient = (id, T, C) => evalJs(`(() => { const svg = document.getElementById('map-${id}'); const g = k => +svg.dataset[k];
  const d = D.domain, lt0 = Math.log(d.t[0]), lt1 = Math.log(d.t[1]), lc0 = Math.log(d.c[0]), lc1 = Math.log(d.c[1]);
  const x = g('x0') + g('w') * (Math.log(${T}) - lt0) / (lt1 - lt0), y = g('y0') + g('h') * (1 - (Math.log(${C}) - lc0) / (lc1 - lc0));
  const pt = svg.createSVGPoint(); pt.x = x; pt.y = y; const q = pt.matrixTransform(svg.getScreenCTM()); return [q.x, q.y, x, y]; })()`);
const hover = async (x, y) => {
  await send('Input.dispatchMouseEvent', { type: 'mouseMoved', x, y });
  return evalJs(`(() => { const t = document.getElementById('tip'); return { shown: t.style.display === 'block', text: t.textContent,
    labels: [...t.querySelectorAll('li b')].map(b => b.textContent).sort() }; })()`);
};

let seed = 12345; const rnd = () => (seed = (seed * 1664525 + 1013904223) % 4294967296) / 4294967296;
for (const P of payload.panels) {
  const [lt0, lt1, lc0, lc1] = [...payload.domain.t, ...payload.domain.c].map(Math.log);
  const near = (T, C) => [...P.greptile, ...P.ever_winners.map(n => P.options.find(o => o.name === n))].some(o =>
    Math.hypot((Math.log(o.t) - Math.log(T)) / (lt1 - lt0) * 556, (Math.log(o.c) - Math.log(C)) / (lc1 - lc0) * 500) < 14);
  const allT = [...new Set(P.options.map(o => o.t))].sort((a, b) => a - b), allC = [...new Set(P.options.map(o => o.c))].sort((a, b) => a - b);
  const edgeClose = (T, C) => allT.some(t => Math.abs(Math.log(T) - Math.log(t)) / (lt1 - lt0) * 556 < 1.6) ||
    allC.some(c => Math.abs(Math.log(C) - Math.log(c)) / (lc1 - lc0) * 500 < 1.6);
  let tested = 0, bad = 0, blank = 0, mismatches = [];
  for (let n = 0; n < 700 && tested < 400; n++) {
    const T = Math.exp(lt0 + rnd() * (lt1 - lt0)), C = Math.exp(lc0 + rnd() * (lc1 - lc0));
    if (near(T, C) || edgeClose(T, C)) continue;
    const [cx, cy] = await toClient(P.id, T, C);
    const got = await hover(cx, cy), want = brute(P, T, C);
    tested++;
    let ok;
    if (!want) { ok = got.shown && got.text.includes('No priced setup fits'); blank++; }
    else ok = got.shown && got.text.includes(`Best: ${want.best.toFixed(2)} expected findings`) && !got.text.includes('DATA MISMATCH') &&
      JSON.stringify(got.labels) === JSON.stringify(want.labels);
    if (!ok) { bad++; if (mismatches.length < 3) mismatches.push([T, C, want, got.text]); }
  }
  check(bad === 0 && tested >= 300, `${P.id}: ${tested} hover probes (${blank} infeasible) equal brute force ${bad ? JSON.stringify(mismatches) : ''}`);
  // Tie list: find a tied cell and confirm every tied option is listed.
  let tieSeen = 0;
  for (const t of allT) for (const c of allC) { const w = brute(P, t, c); if (w && w.labels.length > 1) tieSeen++; }
  console.log(`info ${P.id}: ${tieSeen} threshold cells have a tied best set`);
  for (const [k, g] of P.greptile.entries()) {
    const [cx, cy] = await toClient(P.id, g.t, g.c); const got = await hover(cx, cy);
    check(got.text.includes('reference, not optimized') && got.text.includes(`Actual: ${g.f.toFixed(2)} findings`) && got.text.includes(`$${g.c.toFixed(3)}`),
      `${P.id}: ${g.label} hover shows its own findings ${g.f}, not the background (${got.text.slice(0, 160)})`);
  }
  for (const n of P.ever_winners) {
    const o = P.options.find(x => x.name === n); const [cx, cy] = await toClient(P.id, o.t, o.c); const got = await hover(cx, cy);
    if (!got.text.includes(`${o.label}: ${o.f.toFixed(2)} findings, ${o.t.toFixed(1)} s, $${o.c.toFixed(3)}`)) { check(false, `${P.id}: marker ${n}: ${got.text}`); }
  }
  // Pointer leaves the plot: tooltip hides.
  const out = await hover(5, 5);
  check(!out.shown, `${P.id}: tooltip hides outside the map`);
}
// A tied cell, hovered for real, lists every tied option.
{
  const P = payload.panels[1]; let found = null;
  const allT = [...new Set(P.options.map(o => o.t))].sort((a, b) => a - b), allC = [...new Set(P.options.map(o => o.c))].sort((a, b) => a - b);
  const markers = [...P.greptile, ...P.ever_winners.map(n => P.options.find(o => o.name === n))];
  const [lt0, lt1, lc0, lc1] = [...payload.domain.t, ...payload.domain.c].map(Math.log);
  // First tied cell whose midpoint is at least 14 px from every marker, so the budget tooltip shows.
  for (let i = 0; i + 1 < allT.length && !found; i++) for (let j = 0; j + 1 < allC.length && !found; j++) {
    const T = Math.sqrt(allT[i] * allT[i + 1]), C = Math.sqrt(allC[j] * allC[j + 1]), w = brute(P, T, C);
    const far = markers.every(o => Math.hypot((Math.log(o.t) - Math.log(T)) / (lt1 - lt0) * 556, (Math.log(o.c) - Math.log(C)) / (lc1 - lc0) * 500) >= 14);
    if (w && w.labels.length > 1 && far) found = [T, C, w];
  }
  const [cx, cy] = await toClient(P.id, found[0], found[1]); const got = await hover(cx, cy);
  const want = brute(P, found[0], found[1]);
  check(JSON.stringify(got.labels) === JSON.stringify(want.labels) && want.labels.length > 1 && got.text.includes(`${want.labels.length} setups tie`),
    `medium-high: tied cell lists ${want.labels.length} setups: ${want.labels.join(' | ')}`);
  // Pin: click, move away, tooltip stays; Escape hides.
  await send('Input.dispatchMouseEvent', { type: 'mousePressed', x: cx, y: cy, button: 'left', clickCount: 1 });
  await send('Input.dispatchMouseEvent', { type: 'mouseReleased', x: cx, y: cy, button: 'left', clickCount: 1 });
  const before = await hover(cx, cy); const moved = await hover(cx + 60, cy - 40);
  check(before.text === moved.text, 'click pins the tooltip against pointer movement');
  await send('Input.dispatchKeyEvent', { type: 'keyDown', key: 'Escape', code: 'Escape', windowsVirtualKeyCode: 27 });
  check(!(await evalJs(`document.getElementById('tip').style.display === 'block'`)), 'Escape clears the pinned tooltip');
}
// Pin ownership is global: a pin in All survives hover and leave in the other map; a click switches; Escape clears all.
{
  const [A, M] = payload.panels;
  const tipState = () => evalJs(`({ shown: document.getElementById('tip').style.display === 'block', text: document.getElementById('tip').textContent })`);
  const crossCount = id => evalJs(`document.querySelector('#map-${id} .cross').childElementCount`);
  const click = async (x, y) => {
    await send('Input.dispatchMouseEvent', { type: 'mousePressed', x, y, button: 'left', clickCount: 1 });
    await send('Input.dispatchMouseEvent', { type: 'mouseReleased', x, y, button: 'left', clickCount: 1 });
  };
  await send('Input.dispatchKeyEvent', { type: 'keyDown', key: 'Escape', code: 'Escape', windowsVirtualKeyCode: 27 });
  const [ax, ay] = await toClient(A.id, 200, 0.15), [mx, my] = await toClient(M.id, 200, 0.15);
  await click(ax, ay);
  const pinnedText = (await tipState()).text;
  check(pinnedText.startsWith(A.title) && await crossCount(A.id) === 2, 'pin in All shows All tooltip and its crosshair');
  await hover(mx, my);
  const moved = await tipState();
  check(moved.shown && moved.text === pinnedText && await crossCount(M.id) === 0, 'moving over the other map keeps the pinned tooltip and draws no second crosshair');
  await hover(5, 5);
  const left = await tipState();
  check(left.shown && left.text === pinnedText && await crossCount(A.id) === 2, 'leaving the other map keeps the pin and its crosshair');
  await click(mx, my);
  const switched = await tipState();
  check(switched.text.startsWith(M.title) && await crossCount(M.id) === 2 && await crossCount(A.id) === 0, 'click in the other map switches the pin and clears the old crosshair');
  await hover(ax, ay);
  check((await tipState()).text === switched.text, 'hover over the first map keeps the switched pin');
  await send('Input.dispatchKeyEvent', { type: 'keyDown', key: 'Escape', code: 'Escape', windowsVirtualKeyCode: 27 });
  check(!(await tipState()).shown && await crossCount(A.id) === 0 && await crossCount(M.id) === 0, 'Escape clears the tooltip and both crosshairs');
  const after = await hover(ax, ay);
  check(after.shown && after.text.startsWith(A.title), 'hover works again after Escape');
}
check(consoleErrors.length === 0, `no console errors or network requests ${consoleErrors.join(' ').slice(0, 300)}`);
{
  const m = await send('Page.getLayoutMetrics'); const height = Math.ceil(m.result.cssContentSize.height);
  const shot = await send('Page.captureScreenshot', { format: 'png', clip: { x: 0, y: 0, width: 1400, height, scale: 1 }, captureBeyondViewport: true });
  fs.writeFileSync(path.join(shots, 'html-full.png'), Buffer.from(shot.result.data, 'base64'));
}
// A hover screenshot for the record.
{
  const P = payload.panels[0]; const [cx, cy] = await toClient(P.id, 100, 0.3); await hover(cx, cy);
  const shot = await send('Page.captureScreenshot', { format: 'png', clip: { x: 0, y: 0, width: 1400, height: 900, scale: 1 } });
  fs.writeFileSync(path.join(shots, 'html-hover.png'), Buffer.from(shot.result.data, 'base64'));
}
proc.kill();
await new Promise(r => setTimeout(r, 300));
fs.rmSync(profile, { recursive: true, force: true });
process.exit(failures ? 1 : 0);
