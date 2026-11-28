import { useEffect, useMemo, useRef, useState } from 'react';
import { RefreshCw } from 'lucide-react';
import { getUsageReport } from '../../api/client';
import type { SessionUsageReport } from '../../api/types';
import { useSessionStore } from '../../stores/useSessionStore';
import { money, providerRows } from './usageMath';

const HOUR = 3600000;
const pins = (n: number) => n < 20 ? String.fromCodePoint(0x2460 + n) : `(${n + 1})`;
const columns = 'grid min-w-[760px] grid-cols-[minmax(140px,1fr)_minmax(180px,2fr)_80px_100px_90px_minmax(160px,1fr)] items-center gap-3';

function Gauge({ pace, thin = false, payg = false }: { pace: number | null; thin?: boolean; payg?: boolean }) {
  const color = pace === null ? 'bg-muted' : pace < 80 ? 'bg-accent' : pace <= 105 ? 'bg-ok' : pace <= 130 ? 'bg-warn' : 'bg-danger';
  return (
    <div className={`relative w-full rounded bg-raised ${thin ? 'h-1.5' : 'h-3'}`}>
      <div className="absolute inset-y-[-3px] left-1/2 z-10 border-l border-muted/60" />
      {pace === null
        ? <div className={`h-full w-full border-t border-dashed ${payg ? 'border-accent' : 'border-muted'}`} />
        : <div className={`h-full rounded ${color}`} style={{ width: `${Math.min(100, pace / 2)}%` }} />}
    </div>
  );
}

export default function UsageView() {
  const profiles = useSessionStore(s => s.meta?.profiles) ?? [];
  const [report, setReport] = useState<SessionUsageReport | null>(null);
  const [profile, setProfile] = useState('');
  const [suspended, setSuspended] = useState(false);
  const [selected, setSelected] = useState<number[] | null>(null);
  const [refresh, setRefresh] = useState(0);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [error, setError] = useState('');
  const [preview, setPreview] = useState<number[] | null>(null);
  const [dragging, setDragging] = useState(false);
  const anchor = useRef<number | null>(null);
  const drag = useRef<{ start: number; end: number } | null>(null);
  const generation = useRef(0);

  useEffect(() => {
    const controller = new AbortController();
    const current = ++generation.current;
    setError('');
    getUsageReport({ profile: profile || undefined, include_suspended: suspended, hour_starts_ms: selected ?? undefined }, controller.signal)
      .then(value => { if (current === generation.current) setReport(value); })
      .catch(e => { if (!controller.signal.aborted && current === generation.current) setError(String(e)); });
    return () => controller.abort();
  }, [profile, suspended, selected, refresh]);

  const hours = report?.hours ?? [];
  const range = (a: number, b: number) => hours.map(h => h.start_ms)
    .filter(h => h >= Math.min(a, b) && h <= Math.max(a, b));
  const duration = !report ? 0 : selected === null
    ? Math.max(0, (report.until_ms - report.since_ms) / 1000)
    : selected.reduce((s, h) => s + Math.max(0, Math.min(h + HOUR - 1, report.until_ms) - h + 1) / 1000, 0);
  const rows = useMemo(() => report ? providerRows(report, profiles, duration) : [], [report, profiles, duration]);
  const selection = preview ?? selected;

  useEffect(() => {
    if (!dragging) return;
    const finish = (event: PointerEvent) => {
      const current = drag.current;
      if (!current) return;
      if (event.type === 'pointerup') {
        setSelected(hours.map(h => h.start_ms).filter(h => h >= Math.min(current.start, current.end) && h <= Math.max(current.start, current.end)));
        anchor.current = current.start;
      }
      drag.current = null;
      setPreview(null);
      setDragging(false);
    };
    window.addEventListener('pointerup', finish);
    window.addEventListener('pointercancel', finish);
    return () => {
      window.removeEventListener('pointerup', finish);
      window.removeEventListener('pointercancel', finish);
    };
  }, [dragging, hours]);

  function choose(hour: number, shift: boolean, toggle: boolean) {
    const base = selected ?? hours.map(h => h.start_ms);
    if (shift && anchor.current !== null) {
      const values = range(anchor.current, hour);
      setSelected(toggle ? [...new Set([...base, ...values])].sort((a, b) => a - b) : values);
    } else if (toggle) {
      setSelected(base.includes(hour) ? base.filter(h => h !== hour) : [...base, hour].sort((a, b) => a - b));
      anchor.current = hour;
    } else {
      setSelected([hour]);
      anchor.current = hour;
    }
  }

  const max = Math.max(1, ...hours.map(h => h.known_cost_usd_ticks));
  return (
    <main className="h-full overflow-auto bg-bg p-4 text-fg md:p-6">
      <div className="mx-auto max-w-6xl space-y-6">
        <h1 className="text-xl font-semibold">Usage Dashboard</h1>
        <div className="flex flex-wrap items-center gap-3 rounded-lg border border-line bg-surface p-3 text-sm">
          <label>
            Profile <select className="ml-1 rounded border border-line bg-raised p-1" value={profile} onChange={e => setProfile(e.target.value)}>
              <option value="">All</option>
              {[...new Set([...profiles.map(p => p.id), ...(report?.profiles ?? [])])].map(p => <option key={p}>{p}</option>)}
            </select>
          </label>
          <label><input type="checkbox" checked={suspended} onChange={e => setSuspended(e.target.checked)} /> Include suspended sessions</label>
          <span className="rounded bg-raised px-2 py-1">{selected === null ? 'Last 24h' : `${selected.length} selected hours`}</span>
          {selected !== null && <button onClick={() => setSelected(null)} className="text-accent">Show all 24h</button>}
          <button onClick={() => setRefresh(x => x + 1)} className="flex items-center gap-1 rounded border border-line px-2 py-1">
            <RefreshCw className="size-4" /> Refresh
          </button>
          <span className="ml-auto">Estimated API {report ? money(report.cost_usd_ticks) : '…'}</span>
        </div>
        {error && <p role="alert" className="text-danger">{error}</p>}
        {report && <>
          <section className="overflow-x-auto rounded-lg border border-line bg-surface p-4">
            <h2 className="mb-3 font-semibold">Provider Pace</h2>
            <div className={`${columns} pb-2 text-xs text-muted`}>
              <span>Provider</span><span className="flex justify-between"><span>0%</span><span>100%</span><span>200%+</span></span>
              <span>Pace</span><span>Tank / Share</span><span>Est $</span><span>Forecast / Models</span>
            </div>
            {rows.map(row => <div key={row.name} className="border-t border-line py-3">
              <button aria-expanded={expanded.has(row.name)} onClick={() => setExpanded(old => {
                const next = new Set(old);
                next.has(row.name) ? next.delete(row.name) : next.add(row.name);
                return next;
              })} className={`${columns} w-full text-left text-sm`}>
                <span>{expanded.has(row.name) ? '▾' : '▸'} {row.name} <span className="text-muted">{row.hasQuota ? `×${row.accountCount}` : 'API'}</span></span>
                <Gauge pace={row.pace} payg={!row.hasQuota} />
                <span>{row.pace === null ? (row.hasQuota ? 'unknown' : 'payg') : `${row.partial ? '~' : ''}${row.pace}%`}</span>
                <span>{row.tank === null ? (row.hasQuota ? '—' : '∞') : `${Math.round(row.tank)}%`}</span>
                <span>{row.estimatedCost}</span><span>{row.forecast}</span>
              </button>
              {expanded.has(row.name) && row.roles.map(role => <div key={role.name} className={`${columns} mt-3 text-xs`}>
                <span className="pl-4">{role.name}</span><Gauge pace={role.pace} thin />
                <span>{role.pace}%</span><span>{role.share}% share</span><span>${(role.cost / 1e10).toFixed(2)}</span>
                <span>{role.models.map(m => `${m.model ?? 'Unknown'}${m.unreported_turns ? ` (${m.unreported_turns} unreported)` : ''}`).join(', ')}</span>
              </div>)}
            </div>)}
          </section>
          <section className="rounded-lg border border-line bg-surface p-4">
            <h2 className="font-semibold">Last 24 hours</h2>
            <div className="mt-4 flex gap-2">
              <div className="flex h-36 flex-col justify-between text-xs text-muted">
                <span>${(max / 1e10).toFixed(2)}</span><span>${(max / 2e10).toFixed(2)}</span><span>$0</span>
              </div>
              <div className="grid min-w-0 flex-1 touch-none grid-cols-[repeat(24,minmax(0,1fr))] gap-0.5">
                {hours.map(h => <div key={h.start_ms} className="min-w-0">
                  <button
                    aria-label={`Hour ${new Date(h.start_ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}`}
                    aria-pressed={selection?.includes(h.start_ms) ?? false}
                    className={`flex h-36 w-full items-end border-b-2 ${selection?.includes(h.start_ms) ? 'border-accent bg-accent/10' : 'border-line'}`}
                    onClick={e => { if (e.detail === 0) choose(h.start_ms, e.shiftKey, e.ctrlKey || e.metaKey); }}
                    onPointerDown={e => {
                      if (e.shiftKey || e.ctrlKey || e.metaKey) return;
                      drag.current = { start: h.start_ms, end: h.start_ms };
                      setDragging(true);
                      if (e.currentTarget.hasPointerCapture?.(e.pointerId)) e.currentTarget.releasePointerCapture(e.pointerId);
                    }}
                    onPointerEnter={() => {
                      if (drag.current) {
                        drag.current.end = h.start_ms;
                        setPreview(range(drag.current.start, h.start_ms));
                      }
                    }}
                    onPointerUp={e => {
                      if (drag.current) {
                        setSelected(range(drag.current.start, drag.current.end));
                        anchor.current = drag.current.start;
                        drag.current = null;
                        setDragging(false);
                        setPreview(null);
                      } else {
                        choose(h.start_ms, e.shiftKey, e.ctrlKey || e.metaKey);
                      }
                    }}
                  >
                    <span className={`w-full ${h.has_unknown_cost ? 'border border-dashed border-warn bg-warn/30' : 'bg-accent'}`}
                      style={{ height: `${Math.max(h.has_unknown_cost ? 5 : 0, h.known_cost_usd_ticks / max * 100)}%` }} />
                  </button>
                  <div className="h-6 text-center text-xs text-warn">
                    {(report.role_changes ?? []).map((c, i) => Math.floor(c.changed_at_ms / HOUR) * HOUR === h.start_ms ? pins(i) : null)}
                  </div>
                  <div className="overflow-hidden text-[9px] text-muted">{new Date(h.start_ms).toLocaleTimeString([], { hour: 'numeric' })}</div>
                </div>)}
              </div>
            </div>
            {!!report.role_changes?.length && <div className="mt-3 text-sm">
              <h3 className="font-medium">Role changes on timeline</h3>
              {report.role_changes.map((change, i) => {
                const start = Math.floor(change.changed_at_ms / HOUR) * HOUR;
                const postChange = hours.filter(h => h.start_ms >= start).map(h => h.start_ms);
                const active = selected !== null && selected.length === postChange.length && postChange.every(h => selected.includes(h));
                return <button key={change.id} aria-pressed={active} className={`block rounded px-1 py-1 text-left text-accent ${active ? 'bg-accent/15 font-semibold' : ''}`}
                  onClick={() => { setSelected(postChange); anchor.current = start; }}>
                  {pins(i)} {new Date(change.changed_at_ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })} {change.summary}
                </button>;
              })}
            </div>}
          </section>
          <section className="rounded-lg border border-line bg-surface p-4">
            <h2 className="font-semibold">Sessions</h2>
            {report.sessions.map(s => <article key={s.session_id} className="border-t border-line py-3 text-sm">
              <div className="font-medium">{s.title} · {s.profile} {s.suspended && <span className="text-warn">Suspended</span>} · {money(s.cost_usd_ticks)}</div>
              <div className="text-muted">Coverage: {s.coverage.full_turn_reports}/{s.coverage.recorded_turns} turns</div>
              {s.models.map((m, i) => <div key={i} className="text-muted">
                {m.model ?? 'Unknown'} · {Object.entries(m.totals).map(([key, v]) => `${key}: ${v.tokens.toLocaleString()}`).join(' · ')} · {money(m.cost_usd_ticks)} {m.unreported_turns ? `· ${m.unreported_turns} unreported` : ''}
              </div>)}
            </article>)}
          </section>
        </>}
      </div>
    </main>
  );
}
