import { render, screen, fireEvent, waitFor } from '@testing-library/react';
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
  await waitFor(() => expect(vi.mocked(getUsageReport).mock.calls.at(-1)?.[0]?.hour_starts_ms).toEqual([]));
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
  expect(screen.getByText('Last 24h')).toBeInTheDocument();
  expect(screen.getByRole('button', { name: /Changed/ })).toHaveAttribute('aria-pressed', 'false');
  reject(new Error('offline'));
  expect(await screen.findByRole('alert')).toHaveTextContent('offline');
  expect(screen.getByText('Last 24h')).toBeInTheDocument();
  expect(screen.queryByRole('status')).not.toBeInTheDocument();
});
