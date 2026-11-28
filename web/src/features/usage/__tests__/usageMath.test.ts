import { describe, expect, it } from 'vitest';
import type { SessionUsageReport, ViewerProfile } from '../../../api/types';
import { providerRows } from '../usageMath';
const profiles = ['a','b'].map(id => ({ id, harness_kind: 'codex' }) as ViewerProfile);
function report(): SessionUsageReport { return { since_ms: 0, until_ms: 3600000, hours: [], profiles: ['a','b'], sessions: [], cost_usd_ticks: {Ok:0}, quota_usage: [ {profile:'a', consumed_basis_points: 100, covered_seconds: 6048, partial:false, monthly_price_usd:100}, {profile:'b', consumed_basis_points:200, covered_seconds:6048, partial:false, monthly_price_usd:300} ] }; }
describe('provider pace', () => {
  it('weights covered rates by plan price and marks partial coverage', () => { const row = providerRows(report(), profiles, 7200)[0]; expect(row.pace).toBe(175); expect(row.partial).toBe(true); });
  it('excludes short windows and models reset safety across the week', () => { const r = report(); r.quota_projections = [{profile_id:'a', window_label:'5h', is_short_window:true, remaining_percent:0, resets_at_epoch_seconds:1, observed_pct_per_hour:null,sustainable_pct_per_hour:null,pace_ratio:null,projected_exhaustion_epoch_seconds:null}, {profile_id:'a',window_label:'Weekly',remaining_percent:100,resets_at_epoch_seconds:3600,observed_pct_per_hour:null,sustainable_pct_per_hour:null,pace_ratio:null,projected_exhaustion_epoch_seconds:null}]; const row = providerRows(r, profiles, 6048, 0)[0]; expect(row.tank).toBe(100); expect(row.forecast).toBe('dry in 4d 1h'); });
  it('allocates rounded role pace and shares and displays external usage', () => { const r = report(); expect(providerRows(r, profiles, 6048)[0].roles[0].name).toBe('External / untracked'); r.profile_role_usage = ['primary','oracle','reviewer'].map(role => ({profile:'a', role, cost_usd_ticks:{Ok:1}, models:[{model:role, cost_usd_ticks:{Ok:1}, totals:{}}]})); const row = providerRows(r, profiles, 6048)[0]; expect(row.roles.reduce((n,x) => n+(x.pace ?? 0),0)).toBe(row.pace); expect(row.roles.reduce((n,x) => n+x.share,0)).toBe(100); });
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
it('groups by resolved quota provider before harness kind', () => {
  const r = report(); r.profiles = ['a', 'b', 'c'];
  const accounts = [{ id: 'a', harness_kind: 'codex' }, { id: 'b', harness_kind: 'opencode' }, { id: 'c', harness_kind: 'opencode' }] as ViewerProfile[];
  r.profile_providers = { a: ' DeepSeek ', b: 'Anthropic', c: 'Anthropic' };
  r.quota_usage = r.profiles.map(profile => ({ profile, consumed_basis_points: 100, covered_seconds: 6048, partial: false, monthly_price_usd: 100 }));
  expect(providerRows(r, accounts, 6048).map(row => [row.name, row.accountCount])).toEqual([['DeepSeek', 1], ['Anthropic', 2]]);
});
it('retains priced model costs for role shares when another model is unknown', () => {
  const r = report();
  r.profile_role_usage = [
    { profile: 'a', role: 'primary', cost_usd_ticks: { Err: null }, models: [{ model: 'priced', totals: {}, cost_usd_ticks: { Ok: 30000000000 } }, { model: 'unknown', totals: {}, cost_usd_ticks: { Err: null } }] },
    { profile: 'a', role: 'oracle', cost_usd_ticks: { Ok: 10000000000 }, models: [{ model: 'other', totals: {}, cost_usd_ticks: { Ok: 10000000000 } }] },
  ];
  expect(providerRows(r, profiles, 6048)[0].roles.map(role => [role.cost, role.unknownCost, role.share, role.pace])).toEqual([[30000000000, true, 75, 131], [10000000000, false, 25, 44]]);
});
it('uses total tokens alone for fallback shares', () => {
  const r = report(); const total = (tokens: number) => ({ tokens, reported_turns: 1 });
  r.profile_role_usage = [
    { profile: 'a', role: 'primary', cost_usd_ticks: { Err: null }, models: [{ model: 'a', cost_usd_ticks: { Err: null }, totals: { input_tokens: total(90), output_tokens: total(10), total_tokens: total(100) } }] },
    { profile: 'a', role: 'oracle', cost_usd_ticks: { Err: null }, models: [{ model: 'b', cost_usd_ticks: { Err: null }, totals: { total_tokens: total(100) } }] },
  ];
  expect(providerRows(r, profiles, 6048)[0].roles.map(role => role.share)).toEqual([50, 50]);
});
it('forecasts exhaustion at an on-pace rate with a low tank', () => {
  const r = report(); r.profiles = ['a']; r.quota_usage = [r.quota_usage![0]];
  r.quota_projections = [{ profile_id: 'a', window_label: 'Weekly', remaining_percent: 10, resets_at_epoch_seconds: 3 * 86400, observed_pct_per_hour: null, sustainable_pct_per_hour: null, pace_ratio: null, projected_exhaustion_epoch_seconds: null }];
  const row = providerRows(r, profiles, 6048, 0)[0]; expect(row.pace).toBe(100); expect(row.forecast).toMatch(/^dry in /);
});

it('does not create provider rows from unfiltered dropdown profiles', () => {
  const r = report(); r.quota_usage = [r.quota_usage![0]];
  r.profile_providers = { a: 'OpenAI', b: 'Anthropic' };
  expect(providerRows(r, profiles, 6048).map(row => row.name)).toEqual(['OpenAI']);
});
it('excludes explicitly nonweekly windows and drains an account without a reset', () => {
  const r = report(); r.quota_usage = [r.quota_usage![0]];
  const base = { profile_id: 'a', remaining_percent: 10, resets_at_epoch_seconds: null, observed_pct_per_hour: null, sustainable_pct_per_hour: null, pace_ratio: null, projected_exhaustion_epoch_seconds: null };
  r.quota_projections = [{ ...base, window_label: 'Weekly', is_weekly_window: false, remaining_percent: 99 }, { ...base, window_label: 'Plan', is_weekly_window: true }];
  const row = providerRows(r, profiles, 6048, 0)[0];
  expect(row.tank).toBe(10); expect(row.forecast).toBe('dry in 16h 48m');
  r.quota_projections = [{ ...base, window_label: 'Daily' }];
  expect(providerRows(r, profiles, 6048, 0)[0].tank).toBeNull();
});
it('allocates subscription pace only to subscription roles', () => {
  const r = report(); r.quota_usage = [r.quota_usage![0]];
  r.profile_role_usage = ['a', 'b'].map((profile, i) => ({ profile, role: i ? 'oracle' : 'primary', cost_usd_ticks: { Ok: 1 }, models: [{ model: 'm', cost_usd_ticks: { Ok: i ? 9 : 1 }, totals: {} }] }));
  expect(providerRows(r, profiles, 6048)[0].roles.map(role => role.pace)).toEqual([100, 0]);
  r.profile_role_usage = r.profile_role_usage.slice(1);
  expect(providerRows(r, profiles, 6048)[0].roles.map(role => [role.name, role.pace])).toEqual([['Oracles', 0], ['External / untracked', 100]]);
});
