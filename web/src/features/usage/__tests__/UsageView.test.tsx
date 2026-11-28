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
