import { render, screen, fireEvent, waitFor, within } from '@testing-library/react';
import { beforeEach, expect, it, vi } from 'vitest';
import UsageView from '../UsageView';
import { getUsageReport } from '../../../api/client';
vi.mock('../../../api/client', () => ({ getUsageReport: vi.fn() }));
const start = Math.floor(Date.now() / 3600000) * 3600000 - 23 * 3600000;
const hours = Array.from({length:24}, (_, i) => ({start_ms:start+i*3600000, known_cost_usd_ticks:100, has_unknown_cost:false}));
const report = {since_ms:start, until_ms:Date.now(), hours, profiles:['a'], sessions:[{session_id:'s',title:'Session one',profile:'a',suspended:false,subagent_count:0,models:[],coverage:{recorded_turns:1,full_turn_reports:1},cost_usd_ticks:{Ok:0}}], cost_usd_ticks:{Ok:0}, role_changes:[{id:1,changed_at_ms:start+5*3600000,summary:'Changed'}], quota_usage:[{profile:'a',consumed_basis_points:100,covered_seconds:6048,monthly_price_usd:100,partial:false}]};
beforeEach(() => { window.PointerEvent = MouseEvent as typeof PointerEvent; vi.mocked(getUsageReport).mockReset().mockResolvedValue(report as never); });
it('expands roles and selects post-change hours while retaining sessions', async () => { render(<UsageView />); const provider = await screen.findByRole('button',{name:/▸ a/}); fireEvent.click(provider); expect(screen.getByText('External / untracked')).toBeInTheDocument(); fireEvent.click(provider); expect(screen.queryByText('External / untracked')).not.toBeInTheDocument(); expect(screen.getByText(/Session one/)).toBeInTheDocument(); expect(screen.queryByText('Role totals')).not.toBeInTheDocument(); fireEvent.click(screen.getByRole('button',{name:/Changed/})); await waitFor(() => expect(vi.mocked(getUsageReport).mock.calls.at(-1)?.[0]?.hour_starts_ms).toEqual(hours.slice(5).map(h => h.start_ms))); });
it('selects single, toggle, shift and dragged ranges', async () => { render(<UsageView />); await screen.findByText('Sessions'); const bars = screen.getAllByRole('button',{name:/^Hour /}); fireEvent.pointerUp(bars[1]); await waitFor(() => expect(vi.mocked(getUsageReport).mock.calls.at(-1)?.[0]?.hour_starts_ms).toEqual([hours[1].start_ms])); fireEvent.pointerUp(bars[3],{ctrlKey:true}); await waitFor(() => expect(vi.mocked(getUsageReport).mock.calls.at(-1)?.[0]?.hour_starts_ms).toEqual([hours[1].start_ms,hours[3].start_ms])); fireEvent.pointerUp(bars[5],{shiftKey:true}); await waitFor(() => expect(vi.mocked(getUsageReport).mock.calls.at(-1)?.[0]?.hour_starts_ms).toEqual(hours.slice(3,6).map(h => h.start_ms))); fireEvent.pointerDown(bars[7]); fireEvent.pointerEnter(bars[9]); fireEvent.pointerUp(bars[9]); await waitFor(() => expect(vi.mocked(getUsageReport).mock.calls.at(-1)?.[0]?.hour_starts_ms).toEqual(hours.slice(7,10).map(h => h.start_ms))); });
it('toggles an hour off from all hours and commits a drag released outside', async () => {
  render(<UsageView />);
  await screen.findByText('Sessions');
  const bars = screen.getAllByRole('button', { name: /^Hour / });
  fireEvent.pointerUp(bars[2], { ctrlKey: true });
  await waitFor(() => expect(vi.mocked(getUsageReport).mock.calls.at(-1)?.[0]?.hour_starts_ms)
    .toEqual(hours.filter((_, i) => i !== 2).map(h => h.start_ms)));
  fireEvent.pointerDown(bars[7]);
  fireEvent.pointerEnter(bars[10]);
  fireEvent.pointerUp(window);
  await waitFor(() => expect(vi.mocked(getUsageReport).mock.calls.at(-1)?.[0]?.hour_starts_ms)
    .toEqual(hours.slice(7, 11).map(h => h.start_ms)));
});
it('displays partial role costs and highlights exhaustion at 100 percent pace', async () => {
  vi.mocked(getUsageReport).mockResolvedValue({ ...report,
    quota_projections: [{ profile_id: 'a', window_label: 'Weekly', remaining_percent: 10, resets_at_epoch_seconds: Date.now() / 1000 + 3 * 86400, observed_pct_per_hour: null, sustainable_pct_per_hour: null, pace_ratio: null, projected_exhaustion_epoch_seconds: null }],
    profile_role_usage: [{ profile: 'a', role: 'primary', cost_usd_ticks: { Err: null }, models: [{ model: 'priced', cost_usd_ticks: { Ok: 30000000000 }, totals: {} }, { model: 'unknown', cost_usd_ticks: { Err: null }, totals: {} }] }],
  } as never);
  render(<UsageView />); expect(await screen.findByText(/^dry in /)).toHaveClass('text-danger');
  fireEvent.click(screen.getByRole('button', { name: /▸ a/ })); expect(screen.getByText('$3.00+')).toBeInTheDocument();
});
it('clears rolled-off selections and anchors before a shift selection', async () => {
  const advancedHours = [...hours.slice(1), { ...hours[0], start_ms: hours[23].start_ms + 3600000 }];
  render(<UsageView />); await screen.findByText('Sessions');
  fireEvent.pointerUp(screen.getAllByRole('button', { name: /^Hour / })[0]);
  await waitFor(() => expect(vi.mocked(getUsageReport).mock.calls.at(-1)?.[0]?.hour_starts_ms).toEqual([hours[0].start_ms]));
  vi.mocked(getUsageReport).mockResolvedValue({ ...report, hours: advancedHours } as never);
  fireEvent.click(screen.getByRole('button', { name: /Refresh/ }));
  await waitFor(() => expect(vi.mocked(getUsageReport).mock.calls.at(-1)?.[0]?.hour_starts_ms).toBeUndefined());
  expect(screen.getByText(/Last 24h/)).toBeInTheDocument();
  fireEvent.pointerUp(screen.getAllByRole('button', { name: /^Hour / })[2], { shiftKey: true });
  await waitFor(() => expect(vi.mocked(getUsageReport).mock.calls.at(-1)?.[0]?.hour_starts_ms).toEqual([advancedHours[2].start_ms]));
});

it.each([['payg', []], ['unknown', [{ profile: 'a', consumed_basis_points: null, covered_seconds: 6048, monthly_price_usd: 100, partial: false }]]])('renders %s for expanded unmeasured roles', async (label, quota) => {
  vi.mocked(getUsageReport).mockResolvedValue({ ...report, quota_usage: quota, profile_role_usage: [{ profile: 'a', role: 'primary', cost_usd_ticks: { Ok: 1 }, models: [{ model: 'm', cost_usd_ticks: { Ok: 1 }, totals: {} }] }] } as never);
  render(<UsageView />); fireEvent.click(await screen.findByRole('button', { name: /▸ a/ }));
  expect(screen.getAllByText(label)).toHaveLength(2);
  expect(screen.getByText('Primary agents').parentElement).toHaveTextContent(label);
});
it('keeps loaded window metadata while a selection loads and after failure', async () => {
  render(<UsageView />); await screen.findByText('Sessions');
  let reject!: (reason: Error) => void;
  vi.mocked(getUsageReport).mockImplementationOnce(() => new Promise((_, fail) => { reject = fail; }));
  fireEvent.click(screen.getByRole('button', { name: /Changed/ }));
  expect(await screen.findByRole('status')).toHaveTextContent('Loading…');
  expect(screen.getByText(/Last 24h/)).toBeInTheDocument();
  expect(screen.getByRole('button', { name: /Changed/ })).toHaveAttribute('aria-pressed', 'false');
  reject(new Error('offline'));
  expect(await screen.findByRole('alert')).toHaveTextContent('offline');
  expect(screen.getByText(/Last 24h/)).toBeInTheDocument();
  expect(screen.queryByRole('status')).not.toBeInTheDocument();
});


it('renders session and model usage in aligned table columns', async () => {
  const token = (tokens: number) => ({ tokens, reported_turns: 1 });
  vi.mocked(getUsageReport).mockResolvedValue({ ...report, sessions: [{
    ...report.sessions[0], suspended: true, subagent_count: 2,
    coverage: { recorded_turns: 4, full_turn_reports: 2, unfinished_turns: 1 },
    cost_usd_ticks: { Ok: 82000000000 },
    models: [
      { model: 'Model A', cost_usd_ticks: { Ok: 82000000000 }, unreported_turns: 2,
        totals: { total_tokens: token(1200), input_tokens: token(900), output_tokens: token(300), cached_read_tokens: token(200), cached_write_tokens: token(50), thought_tokens: token(20) } },
      { model: null, cost_usd_ticks: { Err: null }, totals: { total_tokens: token(500), input_tokens: token(400) } },
    ],
  }] } as never);
  render(<UsageView />);
  const table = await screen.findByRole('table', { name: 'Sessions' });
  expect(within(table).getAllByRole('columnheader').map(header => header.textContent)).toEqual(['Session / Model', 'Profile', 'Est $', 'Tokens', 'Input', 'Output', 'Cached', 'Coverage']);
  expect(within(table).getAllByRole('row')).toHaveLength(2);
  fireEvent.click(within(table).getByRole('button', { name: 'Expand Session one' }));
  const rows = within(table).getAllByRole('row');
  expect(within(rows[1]).getAllByRole('cell').map(cell => cell.textContent)).toEqual(['a', '$8.20+', '1,700+', '1,300+', '300+', '250+', '2 sub · 2/4 turns · 1 active']);
  expect(within(rows[1]).getByText('Suspended')).toBeInTheDocument();
  expect(within(rows[1]).getByTitle('Read: 200 · Write: 50 · Thought: 20')).toHaveTextContent('250+');
  expect(within(rows[2]).getByRole('rowheader')).toHaveTextContent('├─ Model A');
  expect(within(rows[2]).getByRole('rowheader')).toHaveClass('pl-6');
  expect(within(rows[2]).getAllByRole('cell').map(cell => cell.textContent)).toEqual(['', '$8.20+', '1,200+', '900+', '300+', '250+', '2 unreported · thought: 20']);
  expect(within(rows[3]).getByRole('rowheader')).toHaveTextContent('└─ Unknown model');
  expect(within(rows[3]).getAllByRole('cell').map(cell => cell.textContent)).toEqual(['', '?', '500', '400', '—', '—', '']);
  fireEvent.click(within(table).getByRole('button', { name: 'Collapse Session one' }));
  expect(within(table).getAllByRole('row')).toHaveLength(2);
});

it('shows an empty sessions table when no sessions match', async () => {
  vi.mocked(getUsageReport).mockResolvedValue({ ...report, sessions: [] } as never);
  render(<UsageView />);
  const table = await screen.findByRole('table', { name: 'Sessions' });
  expect(within(table).getByText('No sessions match these filters.')).toHaveAttribute('colspan', '8');
});

it('clears the last hour by toggle, pointer and keyboard click, and toggles role changes', async () => {
  render(<UsageView />); await screen.findByText('Sessions');
  const bar = screen.getAllByRole('button', { name: /^Hour / })[1];
  for (const deselect of [() => fireEvent.pointerUp(bar, { ctrlKey: true }), () => { fireEvent.pointerDown(bar); fireEvent.pointerUp(bar); }, () => fireEvent.click(bar, { detail: 0 })]) {
    fireEvent.pointerUp(bar);
    await waitFor(() => expect(vi.mocked(getUsageReport).mock.calls.at(-1)?.[0]?.hour_starts_ms).toEqual([hours[1].start_ms]));
    deselect();
    await waitFor(() => expect(vi.mocked(getUsageReport).mock.calls.at(-1)?.[0]?.hour_starts_ms).toBeUndefined());
    expect(screen.getByText(/Last 24h/)).toBeInTheDocument();
    expect(screen.queryByText('Show all 24h')).not.toBeInTheDocument();
  }
  const role = screen.getByRole('button', { name: /Changed/ });
  fireEvent.click(role); await waitFor(() => expect(role).toHaveAttribute('aria-pressed', 'true'));
  fireEvent.click(role); await waitFor(() => expect(vi.mocked(getUsageReport).mock.calls.at(-1)?.[0]?.hour_starts_ms).toBeUndefined());
});

it('orders sections and collapses provider pace into a sorted over-100 summary', async () => {
  vi.mocked(getUsageReport).mockResolvedValue({ ...report, quota_usage: [
    { profile: 'Zed', consumed_basis_points: 115, covered_seconds: 6048, monthly_price_usd: 100, partial: true },
    { profile: 'Alpha', consumed_basis_points: 142, covered_seconds: 6048, monthly_price_usd: 100, partial: false },
    { profile: 'Low', consumed_basis_points: 100, covered_seconds: 6048, monthly_price_usd: 100, partial: false },
  ] } as never);
  render(<UsageView />);
  const timeline = await screen.findByText('Last 24 hours');
  const total = screen.getByText(/Total.*estimated API USD/);
  const pace = screen.getByRole('button', { name: /Provider Pace/ });
  const sessions = screen.getByText('Sessions');
  expect(timeline.compareDocumentPosition(total) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  expect(total.compareDocumentPosition(pace) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  expect(pace.compareDocumentPosition(sessions) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  expect(pace).toHaveAttribute('aria-expanded', 'true');
  fireEvent.click(pace);
  expect(pace).toHaveAttribute('aria-expanded', 'false');
  const summary = screen.getByText(/Over 100%:/);
  expect(summary).toHaveTextContent('Over 100%: Alpha ~142%, Zed ~115%');
  expect(summary).not.toHaveTextContent('Low');
  expect(screen.queryByText('Tank / Share')).not.toBeInTheDocument();
});

it('shows a muted message when no providers exceed 100 percent', async () => {
  render(<UsageView />);
  fireEvent.click(await screen.findByRole('button', { name: /Provider Pace/ }));
  expect(screen.getByText('No providers over 100%')).toHaveClass('text-muted');
});
