// Drive the budget-map HTML in headless Chrome and compare real hover tooltips with brute force.
// Usage: node audit_budget_html.mjs ARTIFACT_DIR SCREENSHOT_DIR CHROME_HEADLESS_SHELL
// Chrome, the DevTools socket and the temporary profile are released on every exit path, and the
// profile is removed only after the browser's process group has exited.
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

const [artifacts, shots, chrome] = process.argv.slice(2);
const stem = 'review-opus-high-budget';
const payload = JSON.parse(fs.readFileSync(path.join(artifacts, `${stem}-data.json`), 'utf8'));
fs.mkdirSync(shots, { recursive: true });
let failures = 0;
const check = (ok, msg) => { console.log((ok ? 'ok   ' : 'FAIL ') + msg); if (!ok) failures++; };

const profile = fs.mkdtempSync(path.join(shots, 'profile-'));
const pending = new Map();
// First loss of the browser or its DevTools socket, recorded durably. `lostSignal` only ever resolves,
// so it cannot raise an unhandled rejection; every wait races against it and then rethrows `lost`.
let lost = null, onLost;
const lostSignal = new Promise(resolve => { onLost = resolve; });
const lose = err => {
  if (!lost) { lost = err; onLost(err); }
  for (const p of pending.values()) p.reject(err);
  pending.clear();
};
const orLost = async promise => { const value = await Promise.race([promise, lostSignal]); if (lost) throw lost; return value; };
let proc = null, ws = null, exited = null, failure = null;
const cleanupErrors = [];
try {
  proc = spawn(chrome, ['--headless', '--no-sandbox', '--disable-gpu', '--remote-debugging-port=0',
    '--user-data-dir=' + profile, '--window-size=1400,1500', 'about:blank'],
    { stdio: ['ignore', 'ignore', 'pipe'], detached: true });  // own process group, killed as a unit
  // Observe exit from the first moment, so an early exit cannot be missed.
  exited = new Promise(resolve => proc.once('exit', (code, signal) => {
    lose(new Error(`chrome exited (${code ?? signal}) during the audit`));
    resolve({ code, signal });
  }));
  await new Promise((resolve, reject) => { proc.once('spawn', resolve); proc.once('error', reject); });
  const port = await new Promise((resolve, reject) => {
    let buf = '';
    proc.stderr.on('data', d => { buf += d; const m = buf.match(/ws:\/\/127\.0\.0\.1:(\d+)\//); if (m) resolve(m[1]); });
    exited.then(r => reject(new Error(`chrome exited (${r.code ?? r.signal}) before listening: ${buf.slice(0, 300)}`)));
  });
  const targets = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
  ws = new WebSocket(targets.find(t => t.type === 'page').webSocketDebuggerUrl);
  await new Promise((resolve, reject) => { ws.addEventListener('open', resolve); ws.addEventListener('error', () => reject(new Error('devtools socket error'))); });
  ws.addEventListener('close', () => lose(new Error('devtools socket closed')));
  let nextId = 1; const consoleErrors = [];
  let onLoad; const loaded = new Promise(resolve => { onLoad = resolve; });
  ws.addEventListener('message', ev => {
    const m = JSON.parse(ev.data);
    if (m.id && pending.has(m.id)) { pending.get(m.id).resolve(m); pending.delete(m.id); }
    else if (m.method === 'Page.loadEventFired') onLoad();
    else if (m.method === 'Runtime.exceptionThrown') consoleErrors.push(JSON.stringify(m.params.exceptionDetails));
    else if (m.method === 'Runtime.consoleAPICalled' && m.params.type === 'error') consoleErrors.push(JSON.stringify(m.params.args));
    else if (m.method === 'Network.requestWillBeSent' && !m.params.request.url.startsWith('file:')) consoleErrors.push('network: ' + m.params.request.url);
  });
  const send = (method, params = {}) => new Promise((resolve, reject) => {
    if (lost) { reject(lost); return; }
    const id = nextId++; pending.set(id, { resolve, reject }); ws.send(JSON.stringify({ id, method, params }));
  });
  const evalJs = async expr => { const r = await send('Runtime.evaluate', { expression: expr, returnByValue: true }); if (r.result.exceptionDetails) throw new Error(JSON.stringify(r.result.exceptionDetails)); return r.result.result.value; };
  await send('Page.enable'); await send('Runtime.enable'); await send('Network.enable');
  await send('Emulation.setDeviceMetricsOverride', { width: 1400, height: 1500, deviceScaleFactor: 1, mobile: false });
  const pageUrl = pathToFileURL(path.resolve(artifacts, `${stem}.html`)).href;
  const nav = await send('Page.navigate', { url: pageUrl });
  // Fail on a reported navigation error before waiting for a load event that will not come.
  if (nav.error || nav.result.errorText) throw new Error(`navigation to ${pageUrl} failed: ${nav.error ? JSON.stringify(nav.error) : nav.result.errorText}`);
  await orLost(loaded);
  const landed = await evalJs('location.href');
  if (landed !== pageUrl) throw new Error(`navigated to ${landed}, expected ${pageUrl}`);

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

  // Independent oracle for the tooltip's budget band: closed lower, open upper, from the option thresholds.
  const bandSpan = (vals, v, fmt) => {
    let i = -1; for (let k = 0; k < vals.length; k++) if (vals[k] <= v) i = k;
    if (i < 0) return `below ${fmt(vals[0])}`;
    return i + 1 < vals.length ? `${fmt(vals[i])} to below ${fmt(vals[i + 1])}` : `${fmt(vals[i])} and above`;
  };
  const distinctOf = (P, key) => [...new Set(P.options.map(o => o[key]))].sort((a, b) => a - b);
  const expectedHeader = (P, T, C) => `${P.title}: budget latency ${bandSpan(distinctOf(P, 't'), T, v => v.toFixed(1) + ' s')}, cost ${bandSpan(distinctOf(P, 'c'), C, v => '$' + v.toFixed(3))}`;
  const f64 = new Float64Array(1), u64 = new BigUint64Array(f64.buffer);
  const nextDown = x => { f64[0] = x; u64[0] -= 1n; return f64[0]; };
  const svgToClient = (id, x, y) => evalJs(`(() => { const svg = document.getElementById('map-${id}'); const pt = svg.createSVGPoint(); pt.x = ${x}; pt.y = ${y};
    const q = pt.matrixTransform(svg.getScreenCTM()); return [q.x, q.y]; })()`);

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
      if (!want) { ok = got.shown && got.text.includes('No priced setup fits') && got.text.startsWith(expectedHeader(P, T, C)); blank++; }
      else ok = got.shown && got.text.includes(`Best: ${want.best.toFixed(2)} expected findings`) && got.text.startsWith(expectedHeader(P, T, C)) && !got.text.includes('DATA MISMATCH') &&
        JSON.stringify(got.labels) === JSON.stringify(want.labels);
      if (!ok) { bad++; if (mismatches.length < 3) mismatches.push([T, C, want, got.text]); }
    }
    check(bad === 0 && tested >= 300, `${P.id}: ${tested} hover probes (${blank} infeasible) equal brute force ${bad ? JSON.stringify(mismatches) : ''}`);
    // Band header at every observed threshold, one float below it, and both together (evaluated in the page).
    {
      const idx = payload.panels.indexOf(P);
      const pts = [];
      for (const t of allT) for (const c of allC) pts.push([t, c], [nextDown(t), c], [t, nextDown(c)], [nextDown(t), nextDown(c)]);
      // Chunked, and only the header and best line come back, so no single DevTools message is huge.
      const texts = [];
      for (let k = 0; k < pts.length; k += 2000) {
        texts.push(...await evalJs(`(() => { const P = D.panels[${idx}]; return ${JSON.stringify(pts.slice(k, k + 2000))}.map(([T, C]) => {
          const kids = budgetTip(P, T, C).children; return kids[0].textContent + '|' + kids[1].textContent; }); })()`));
      }
      let badBand = 0, first = null;
      pts.forEach(([T, C], k) => {
        const want = brute(P, T, C), text = texts[k];
        const ok = text.startsWith(expectedHeader(P, T, C)) &&
          (want ? text.includes(`Best: ${want.best.toFixed(2)} expected findings`) && !text.includes('No priced setup fits') : text.includes('No priced setup fits'));
        if (!ok) { badBand++; first ||= [T, C, text]; }
      });
      check(badBand === 0, `${P.id}: band header and feasibility at ${pts.length} threshold/just-below points equal the oracle ${badBand ? JSON.stringify(first) : ''}`);
      const [lowT, lowC] = [allT[0], allC[0]];
      const justBelow = await evalJs(`(() => { const P = D.panels[${idx}]; return [budgetTip(P, ${nextDown(lowT)}, ${lowC}).textContent, budgetTip(P, ${lowT}, ${nextDown(lowC)}).textContent, budgetTip(P, ${lowT}, ${lowC}).textContent]; })()`);
      check(justBelow[0].includes(`latency below ${lowT.toFixed(1)} s`) && justBelow[0].includes('No priced setup fits') &&
        justBelow[1].includes(`cost below $${lowC.toFixed(3)}`) && justBelow[1].includes('No priced setup fits') &&
        justBelow[2].includes(`latency ${lowT.toFixed(1)} s to below`) && !justBelow[2].includes('No priced setup fits'),
        `${P.id}: just below ${lowT} s or $${lowC} reads "below" and is infeasible; at the threshold it is feasible`);
      // Real pointer a few pixels either side of the first latency edge.
      for (const f of [0.996, 1.004]) {
        const [cx, cy] = await toClient(P.id, lowT * f, 0.06); const got = await hover(cx, cy), want = brute(P, lowT * f, 0.06);
        check(got.text.startsWith(expectedHeader(P, lowT * f, 0.06)) && (want ? !got.text.includes('No priced') : got.text.includes('No priced setup fits')),
          `${P.id}: pointer at ${(lowT * f).toFixed(2)} s reads "${got.text.slice(0, 90)}"`);
      }
    }
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
  // Marker hits snap the crosshair to the selected setup's own latency and cost, so tooltip and crosshair agree.
  {
    const tipState = () => evalJs(`({ shown: document.getElementById('tip').style.display === 'block', text: document.getElementById('tip').textContent })`);
    const crossLines = id => evalJs(`[...document.querySelectorAll('#map-${id} .cross line')].map(l => ['x1', 'y1', 'x2', 'y2'].map(k => +l.getAttribute(k)))`);
    const snappedAt = (lines, sx, sy) => lines.length === 2 && lines.some(([x1, y1, x2, y2]) => Math.abs(x1 - sx) < 0.01 && Math.abs(x2 - sx) < 0.01) &&
      lines.some(([x1, y1, x2, y2]) => Math.abs(y1 - sy) < 0.01 && Math.abs(y2 - sy) < 0.01);
    const press = async (x, y) => {
      await send('Input.dispatchMouseEvent', { type: 'mousePressed', x, y, button: 'left', clickCount: 1 });
      await send('Input.dispatchMouseEvent', { type: 'mouseReleased', x, y, button: 'left', clickCount: 1 });
    };
    const esc = () => send('Input.dispatchKeyEvent', { type: 'keyDown', key: 'Escape', code: 'Escape', windowsVirtualKeyCode: 27 });
    await esc();
    for (const P of payload.panels) {
      const first = P.options.find(o => o.name === P.ever_winners[0]), other = P.options.find(o => o.name === P.ever_winners[10]);
      const cases = [[`first ${first.label}`, first, -4, 0], [`ordinary ${other.label}`, other, 3, 3],
        ...P.greptile.map(g => [`${g.label} reference`, g, -4, 3])];
      for (const [label, o, dx, dy] of cases) {
        const [cx, cy, sx, sy] = await toClient(P.id, o.t, o.c);
        const got = await hover(cx + dx, cy + dy), lines = await crossLines(P.id);
        const isRef = !!o.name && o.name.startsWith('Greptile');
        const tipOk = isRef ? got.text.includes('reference, not optimized') && got.text.includes(`Actual: ${o.f.toFixed(2)} findings`)
          : got.text.startsWith(`${o.label}: ${o.f.toFixed(2)} findings`) && got.text.includes('Best:') && !got.text.includes('No priced setup fits');
        check(tipOk && snappedAt(lines, sx, sy), `${P.id}: pointer ${dx},${dy} px from ${label} shows its tooltip with the crosshair snapped to its latency and cost`);
      }
      // Click near the marker pins that same snapped selection against later movement.
      const [cx, cy, sx, sy] = await toClient(P.id, first.t, first.c);
      await press(cx - 4, cy);
      const pinnedText = (await tipState()).text;
      await hover(cx + 200, cy - 100);
      check((await tipState()).text === pinnedText && pinnedText.startsWith(first.label) && snappedAt(await crossLines(P.id), sx, sy),
        `${P.id}: a click near ${first.label} pins the snapped selection`);
      await esc();
      // Outside the hit radius the pointer stays the budget: infeasible just left of the first marker.
      const far = await hover(cx - 14, cy), farLines = await crossLines(P.id);
      check(far.text.includes('No priced setup fits') && !snappedAt(farLines, sx, sy) && farLines.length === 2,
        `${P.id}: 14 px left of ${first.label} stays an infeasible budget with the crosshair at the pointer`);
    }
    await hover(5, 5);
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
    // A click on either map's axes, outside its plot, changes neither the pin, the tooltip nor the crosshair.
    const [mox, moy] = await svgToClient(M.id, 20, 200), [aox, aoy] = await svgToClient(A.id, 300, 560);
    for (const [label, ox, oy] of [['the other map', mox, moy], ['its own map', aox, aoy]]) {
      await click(ox, oy);
      const kept = await tipState();
      check(kept.shown && kept.text === pinnedText && await crossCount(A.id) === 2 && await crossCount(M.id) === 0,
        `click on the axes of ${label} keeps the pin, tooltip and crosshair`);
    }
    await hover(mx, my);
    check((await tipState()).text === pinnedText, 'hover still leaves the pin in place after outside clicks');
    await click(mx, my);
    const switched = await tipState();
    check(switched.text.startsWith(M.title) && await crossCount(M.id) === 2 && await crossCount(A.id) === 0, 'click in the other map switches the pin and clears the old crosshair');
    await hover(ax, ay);
    check((await tipState()).text === switched.text, 'hover over the first map keeps the switched pin');
    await send('Input.dispatchKeyEvent', { type: 'keyDown', key: 'Escape', code: 'Escape', windowsVirtualKeyCode: 27 });
    check(!(await tipState()).shown && await crossCount(A.id) === 0 && await crossCount(M.id) === 0, 'Escape clears the tooltip and both crosshairs');
    const after = await hover(ax, ay);
    check(after.shown && after.text.startsWith(A.title), 'hover works again after Escape');
    const [ox, oy] = await svgToClient(A.id, 300, 560); const outside = await hover(ox, oy);
    check(!outside.shown && await crossCount(A.id) === 0, 'unpinned hover outside the plot hides the tooltip and crosshair');
    await click(ox, oy);
    check(!(await tipState()).shown, 'unpinned click outside the plot pins nothing');
    await hover(ax, ay);
    check((await tipState()).shown, 'hover still works after an unpinned outside click');
  }
  // Layout: each map, the color scale and the page fit the viewport, from phone width to desktop.
  for (const [width, height] of [[375, 900], [320, 900], [1400, 1500]]) {
    await send('Emulation.setDeviceMetricsOverride', { width, height, deviceScaleFactor: 1, mobile: false });
    await evalJs(`document.querySelectorAll('details').forEach(d => { d.open = true; })`);
    const m = await evalJs(`(() => { const rect = e => { const r = e.getBoundingClientRect(); return [r.left, r.right, r.top, r.width]; };
      const doc = document.documentElement;
      return { scroll: doc.scrollWidth, body: document.body.scrollWidth, inner: innerWidth,
        maps: [...document.querySelectorAll('svg.map, svg.bar')].map(rect), tables: [...document.querySelectorAll('.scroll')].map(e => [e.scrollWidth, e.clientWidth]) }; })()`);
    const fits = m.scroll <= m.inner && m.body <= m.inner && m.maps.every(([l, r]) => l >= -0.5 && r <= m.inner + 0.5);
    const sideBySide = m.maps.length >= 3 && Math.abs(m.maps[1][2] - m.maps[2][2]) < 1;
    check(fits && m.maps.every(r => r[3] > 100) && (width < 1000 ? !sideBySide : sideBySide),
      `${width}px viewport: page ${m.scroll}px, maps ${m.maps.map(r => Math.round(r[3])).join('/')}px wide, no horizontal overflow, ${sideBySide ? 'two columns' : 'stacked'}`);
    if (width === 375) {
      const shot = await send('Page.captureScreenshot', { format: 'png', captureBeyondViewport: true });
      fs.writeFileSync(path.join(shots, 'html-375.png'), Buffer.from(shot.result.data, 'base64'));
    }
  }
  await send('Emulation.setDeviceMetricsOverride', { width: 1400, height: 1500, deviceScaleFactor: 1, mobile: false });
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
} catch (e) {
  failure = e;
} finally {
  try { if (ws && ws.readyState < 2) ws.close(); } catch (e) { cleanupErrors.push(`close devtools socket: ${e}`); }
  if (proc && proc.pid) {
    try { process.kill(-proc.pid, 'SIGKILL'); } catch (e) { if (e.code !== 'ESRCH') cleanupErrors.push(`kill browser group: ${e}`); }
    try { await exited; } catch (e) { cleanupErrors.push(`wait for browser exit: ${e}`); }
  }
  try { fs.rmSync(profile, { recursive: true, force: true }); } catch (e) { cleanupErrors.push(`remove profile: ${e}`); }
  if (fs.existsSync(profile)) cleanupErrors.push(`profile still exists: ${profile}`);
}
if (failure) console.error('FAIL audit aborted: ' + (failure.stack || failure));
for (const e of cleanupErrors) console.error('FAIL cleanup: ' + e);
process.exitCode = failure || cleanupErrors.length || failures ? 1 : 0;
