import { describe, expect, it } from 'vitest';
import type { SessionUsageReport, ViewerProfile } from '../../../api/types';
import { providerRows } from '../usageMath';
const profiles = ['a','b'].map(id => ({ id, harness_kind: 'codex' }) as ViewerProfile);
function report(): SessionUsageReport { return { since_ms: 0, until_ms: 3600000, hours: [], profiles: ['a','b'], sessions: [], cost_usd_ticks: {Ok:0}, quota_usage: [ {profile:'a', consumed_basis_points: 100, covered_seconds: 6048, partial:false, monthly_price_usd:100}, {profile:'b', consumed_basis_points:200, covered_seconds:6048, partial:false, monthly_price_usd:300} ] }; }
describe('provider pace', () => {
  it('weights covered rates by plan price and marks partial coverage', () => { const row = providerRows(report(), profiles, 7200)[0]; expect(row.pace).toBe(175); expect(row.partial).toBe(true); });
  it('excludes short windows and models reset safety across the week', () => { const r = report(); r.quota_projections = [{profile_id:'a', window_label:'5h', is_short_window:true, remaining_percent:0, resets_at_epoch_seconds:1, observed_pct_per_hour:null,sustainable_pct_per_hour:null,pace_ratio:null,projected_exhaustion_epoch_seconds:null}, {profile_id:'a',window_label:'Weekly',remaining_percent:100,resets_at_epoch_seconds:3600,observed_pct_per_hour:null,sustainable_pct_per_hour:null,pace_ratio:null,projected_exhaustion_epoch_seconds:null}]; const row = providerRows(r, profiles, 6048, 0)[0]; expect(row.tank).toBe(100); expect(row.forecast).toBe('dry in 4d 1h'); });
  it('allocates rounded role pace and shares and displays external usage', () => { const r = report(); expect(providerRows(r, profiles, 6048)[0].roles[0].name).toBe('External / untracked'); r.profile_role_usage = ['primary','oracle','reviewer'].map(role => ({profile:'a', role, cost_usd_ticks:{Ok:1}, models:[]})); const row = providerRows(r, profiles, 6048)[0]; expect(row.roles.reduce((n,x) => n+x.pace,0)).toBe(row.pace); expect(row.roles.reduce((n,x) => n+x.share,0)).toBe(100); });
});
it('does not invent quota when an empty account resets', () => {
  const r = report();
  r.quota_usage!.forEach(q => { q.monthly_price_usd = 100; q.consumed_basis_points = 175; });
  const projection = (profile_id: string, remaining_percent: number, resets_at_epoch_seconds: number | null) => ({
    profile_id, remaining_percent, resets_at_epoch_seconds, window_label: 'Weekly', observed_pct_per_hour: null,
    sustainable_pct_per_hour: null, pace_ratio: null, projected_exhaustion_epoch_seconds: null,
  });
  r.quota_projections = [projection('a', 0, 24 * 3600), projection('b', 100, null)];
  expect(providerRows(r, profiles, 6048, 0)[0].forecast).toBe('dry in 4d 0h');
});
it('merges the same model across provider accounts', () => {
  const r = report();
  r.profile_role_usage = ['a', 'b'].map(profile => ({
    profile, role: 'primary', cost_usd_ticks: { Ok: 5 },
    models: [{ model: 'gpt', cost_usd_ticks: { Ok: 5 }, unreported_turns: 1, totals: { input: { tokens: 7, reported_turns: 1 } } }],
  }));
  const role = providerRows(r, profiles, 6048)[0].roles[0];
  expect(role.name).toBe('Primary agents');
  expect(role.models).toEqual([{ model: 'gpt', cost_usd_ticks: { Ok: 10 }, unreported_turns: 2, totals: { input: { tokens: 14, reported_turns: 2 } } }]);
});
