import { Fragment, useEffect, useMemo, useRef, useState } from 'react';
import { RefreshCw } from 'lucide-react';
import { getUsageReport } from '../../api/client';
import type { ModelUsageEstimate, SessionUsageReport } from '../../api/types';
import { useSessionStore } from '../../stores/useSessionStore';
import { money, providerRows } from './usageMath';

const HOUR = 3600000;
const pins = (n: number) => n < 20 ? String.fromCodePoint(0x2460 + n) : `(${n + 1})`;
const columns = 'grid min-w-[760px] grid-cols-[minmax(140px,1fr)_minmax(180px,2fr)_80px_100px_90px_minmax(160px,1fr)] items-center gap-3';

function tokenSum(models: ModelUsageEstimate[], keys: string[]): number | null {
  const values = models.flatMap(model => keys.flatMap(key => model.totals[key] ? [model.totals[key].tokens] : []));
  return values.length ? values.reduce((sum, value) => sum + value, 0) : null;
}

function counterPartial(models: ModelUsageEstimate[], keys: string[], basePartial: boolean): boolean {
  if (basePartial) return true;
  return models.some(model => {
    if ((model.unreported_turns ?? 0) > 0) return true;
    const present = keys.flatMap(key => model.totals[key] ? [model.totals[key]] : []);
    if (!present.length) return true;
    const totalTurns = model.totals.total_tokens?.reported_turns ?? 0;
    return totalTurns > 0 && present.every(counter => counter.reported_turns < totalTurns);
  });
}

function tokenText(value: number | null, partial = false) {
  return value === null ? '—' : `${value.toLocaleString()}${partial ? '+' : ''}`;
}

function cachedTitle(models: ModelUsageEstimate[]) {
  return [['cached_read_tokens', 'Read'], ['cached_write_tokens', 'Write'], ['thought_tokens', 'Thought']]
    .flatMap(([key, label]) => {
      const value = tokenSum(models, [key]);
      return value === null ? [] : [`${label}: ${value.toLocaleString()}`];
    }).join(' · ') || undefined;
}

function costText(cost: SessionUsageReport['cost_usd_ticks'], partial: boolean) {
  return 'Ok' in cost ? `${money(cost)}${partial ? '+' : ''}` : '?';
}

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
  const [appliedSelection, setAppliedSelection] = useState<number[] | null>(null);
  const [loading, setLoading] = useState(true);
  const [refresh, setRefresh] = useState(0);
  const [paceExpanded, setPaceExpanded] = useState(true);
  const [expandedSessions, setExpandedSessions] = useState<Set<string>>(new Set());
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
    setLoading(true);
    getUsageReport({ profile: profile || undefined, include_suspended: suspended, hour_starts_ms: selected ?? undefined }, controller.signal)
      .then(value => { if (current === generation.current) { setReport(value); setAppliedSelection(selected); } })
      .catch(e => { if (!controller.signal.aborted && current === generation.current) setError(String(e)); })
      .finally(() => { if (!controller.signal.aborted && current === generation.current) setLoading(false); });
    return () => controller.abort();
  }, [profile, suspended, selected, refresh]);

  useEffect(() => {
    if (!report) return;
    const visibleHours = new Set(report.hours.map(h => h.start_ms));
    if (anchor.current !== null && !visibleHours.has(anchor.current)) anchor.current = null;
    setSelected(current => {
      if (current === null) return null;
      const reconciled = current.filter(h => visibleHours.has(h));
      if (!reconciled.length) { anchor.current = null; return null; }
      return reconciled.length === current.length ? current : reconciled;
    });
  }, [report]);

  const hours = report?.hours ?? [];
  const range = (a: number, b: number) => hours.map(h => h.start_ms)
    .filter(h => h >= Math.min(a, b) && h <= Math.max(a, b));
  const duration = !report ? 0 : appliedSelection === null
    ? Math.max(0, (report.until_ms - report.since_ms) / 1000)
    : appliedSelection.reduce((s, h) => s + Math.max(0, Math.min(h + HOUR - 1, report.until_ms) - h + 1) / 1000, 0);
  const rows = useMemo(() => report ? providerRows(report, profiles, duration) : [], [report, profiles, duration]);
  const selection = preview ?? selected;
  const reportPartial = report?.sessions.some(s => s.coverage.full_turn_reports < s.coverage.recorded_turns || (s.coverage.unfinished_turns ?? 0) > 0) ?? false;
  const overPace = rows.filter(row => row.pace !== null && row.pace > 100).sort((a, b) => b.pace! - a.pace! || a.name.localeCompare(b.name));

  useEffect(() => {
    if (!dragging) return;
    const finish = (event: PointerEvent) => {
      const current = drag.current;
      if (!current) return;
      if (event.type === 'pointerup') {
        if (current.start === current.end) choose(current.start, false, false);
        else { setSelected(range(current.start, current.end)); anchor.current = current.start; }
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
      const values = base.includes(hour) ? base.filter(h => h !== hour) : [...base, hour].sort((a, b) => a - b);
      setSelected(values.length ? values : null);
      anchor.current = values.length ? hour : null;
    } else {
      const deselect = selected?.length === 1 && selected[0] === hour;
      setSelected(deselect ? null : [hour]);
      anchor.current = deselect ? null : hour;
    }
  }

  const max = Math.max(1, ...hours.map(h => h.known_cost_usd_ticks));
  return (
    <main aria-busy={loading} className="h-full overflow-auto bg-bg p-4 text-fg md:p-6">
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
          {loading && <span role="status">Loading…</span>}
          {selected !== null && <button onClick={() => { setSelected(null); anchor.current = null; }} className="text-accent">Show all 24h</button>}
          <button onClick={() => setRefresh(x => x + 1)} className="flex items-center gap-1 rounded border border-line px-2 py-1">
            <RefreshCw className="size-4" /> Refresh
          </button>
        </div>
        {error && <p role="alert" className="text-danger">{error}</p>}
        {report && <>
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
                        if (drag.current.start === drag.current.end) choose(drag.current.start, false, false);
                        else { setSelected(range(drag.current.start, drag.current.end)); anchor.current = drag.current.start; }
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
                const active = appliedSelection !== null && appliedSelection.length === postChange.length && postChange.every(h => appliedSelection.includes(h));
                return <button key={change.id} aria-pressed={active} className={`block rounded px-1 py-1 text-left text-accent ${active ? 'bg-accent/15 font-semibold' : ''}`}
                  onClick={() => { const clear = active || !postChange.length; setSelected(clear ? null : postChange); anchor.current = clear ? null : start; }}>
                  {pins(i)} {new Date(change.changed_at_ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })} {change.summary}
                </button>;
              })}
            </div>}
          </section>
          <section className="rounded-lg border border-line bg-surface p-4 text-sm">
            Total {costText(report.cost_usd_ticks, reportPartial)} · {appliedSelection === null ? 'Last 24h' : `${appliedSelection.length} selected hours`} · estimated API USD
          </section>
          <section className="overflow-x-auto rounded-lg border border-line bg-surface p-4">
            <h2 className="font-semibold"><button type="button" aria-expanded={paceExpanded} onClick={() => setPaceExpanded(value => !value)}>{paceExpanded ? '▾' : '▸'} Provider Pace</button></h2>
            {!paceExpanded && <div className="truncate text-sm text-muted">{overPace.length ? <>Over 100%: {overPace.map((row, i) => <Fragment key={row.name}>{i > 0 && ', '}<span className={row.pace! <= 130 ? 'text-warn' : 'text-danger'}>{row.name} {row.partial ? '~' : ''}{row.pace}%</span></Fragment>)}</> : 'No providers over 100%'}</div>}
            {paceExpanded && <>
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
                <span>{row.estimatedCost}</span><span className={row.forecast.startsWith('dry ') ? 'text-danger' : undefined}>{row.forecast}</span>
              </button>
              {expanded.has(row.name) && row.roles.map(role => <div key={role.name} className={`${columns} mt-3 text-xs`}>
                <span className="pl-4">{role.name}</span><Gauge pace={role.pace} thin />
                <span>{role.pace === null ? (row.hasQuota ? 'unknown' : 'payg') : `${role.pace}%`}</span><span>{role.share}% share</span><span>{role.unknownCost && role.cost === 0 ? '?' : `$${(role.cost / 1e10).toFixed(2)}${role.unknownCost ? '+' : ''}`}</span>
                <span>{role.models.map(m => `${m.model ?? 'Unknown'}${m.unreported_turns ? ` (${m.unreported_turns} unreported)` : ''}`).join(', ')}</span>
              </div>)}
            </div>)}
            </>}
          </section>
          <section className="rounded-lg border border-line bg-surface p-4">
            <h2 className="font-semibold">Sessions</h2>
            <div className="mt-3 overflow-x-auto">
              <table aria-label="Sessions" className="w-full border-collapse text-sm">
                <thead><tr className="text-xs text-muted">
                  <th scope="col" className="px-2 py-2 text-left">Session / Model</th>
                  <th scope="col" className="px-2 py-2 text-left">Profile</th>
                  {['Est $', 'Tokens', 'Input', 'Output', 'Cached'].map(label => <th key={label} scope="col" className="px-2 py-2 text-right tabular-nums">{label}</th>)}
                  <th scope="col" className="px-2 py-2 text-left">Coverage</th>
                </tr></thead>
                <tbody>
                  {report.sessions.length === 0 && <tr><td colSpan={8} className="border-t border-line px-2 py-3 text-muted">No sessions match these filters.</td></tr>}
                  {report.sessions.map(s => {
                    const partial = s.coverage.full_turn_reports < s.coverage.recorded_turns || (s.coverage.unfinished_turns ?? 0) > 0;
                    const isExpanded = expandedSessions.has(s.session_id);
                    return <Fragment key={s.session_id}>
                      <tr className="border-t border-line bg-raised/40">
                        <th scope="row" className="px-2 py-2 text-left font-medium text-fg">{s.models.length > 0 ? <button type="button" aria-expanded={isExpanded} aria-label={`${isExpanded ? 'Collapse' : 'Expand'} ${s.title}`} onClick={() => setExpandedSessions(old => {
                          const next = new Set(old); next.has(s.session_id) ? next.delete(s.session_id) : next.add(s.session_id); return next;
                        })}>{isExpanded ? '▾' : '▸'}</button> : <span aria-hidden="true" className="inline-block w-3" />} {s.title} {s.suspended && <span className="rounded bg-raised px-1.5 py-0.5 text-xs text-warn">Suspended</span>}</th>
                        <td className="px-2 py-2 text-left">{s.profile}</td>
                        <td className="px-2 py-2 text-right tabular-nums">{costText(s.cost_usd_ticks, partial)}</td>
                        <td className="px-2 py-2 text-right tabular-nums">{tokenText(tokenSum(s.models, ['total_tokens']), counterPartial(s.models, ['total_tokens'], partial))}</td>
                        <td className="px-2 py-2 text-right tabular-nums">{tokenText(tokenSum(s.models, ['input_tokens']), counterPartial(s.models, ['input_tokens'], partial))}</td>
                        <td className="px-2 py-2 text-right tabular-nums">{tokenText(tokenSum(s.models, ['output_tokens']), counterPartial(s.models, ['output_tokens'], partial))}</td>
                        <td title={cachedTitle(s.models)} className="px-2 py-2 text-right tabular-nums">{tokenText(tokenSum(s.models, ['cached_read_tokens', 'cached_write_tokens']), counterPartial(s.models, ['cached_read_tokens', 'cached_write_tokens'], partial))}</td>
                        <td className="px-2 py-2 text-left text-xs text-muted">{s.subagent_count} sub · {s.coverage.full_turn_reports}/{s.coverage.recorded_turns} turns{(s.coverage.unfinished_turns ?? 0) > 0 ? ` · ${s.coverage.unfinished_turns} active` : ''}</td>
                      </tr>
                      {isExpanded && s.models.map((m, i) => <tr key={i} className="border-t border-line/40 text-xs text-muted">
                        <th scope="row" className="py-2 pl-6 pr-2 text-left font-normal">{i === s.models.length - 1 ? '└─' : '├─'} {m.model ?? 'Unknown model'}</th>
                        <td />
                        <td className="px-2 py-2 text-right tabular-nums">{costText(m.cost_usd_ticks, (m.unreported_turns ?? 0) > 0)}</td>
                        <td className="px-2 py-2 text-right tabular-nums">{tokenText(tokenSum([m], ['total_tokens']), counterPartial([m], ['total_tokens'], false))}</td>
                        <td className="px-2 py-2 text-right tabular-nums">{tokenText(tokenSum([m], ['input_tokens']), counterPartial([m], ['input_tokens'], false))}</td>
                        <td className="px-2 py-2 text-right tabular-nums">{tokenText(tokenSum([m], ['output_tokens']), counterPartial([m], ['output_tokens'], false))}</td>
                        <td title={cachedTitle([m])} className="px-2 py-2 text-right tabular-nums">{tokenText(tokenSum([m], ['cached_read_tokens', 'cached_write_tokens']), counterPartial([m], ['cached_read_tokens', 'cached_write_tokens'], false))}</td>
                        <td className="px-2 py-2 text-left text-xs text-muted">{[(m.unreported_turns ?? 0) > 0 ? `${m.unreported_turns} unreported` : '', (m.totals.thought_tokens?.tokens ?? 0) > 0 ? `thought: ${m.totals.thought_tokens.tokens.toLocaleString()}` : ''].filter(Boolean).join(' · ')}</td>
                      </tr>)}
                    </Fragment>;
                  })}
                </tbody>
              </table>
            </div>
          </section>
        </>}
      </div>
    </main>
  );
}
