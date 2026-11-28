import type { CostUsdTicks, ModelUsageEstimate, SessionUsageReport, ViewerProfile } from '../../api/types';

export const ticks = (cost: CostUsdTicks) => 'Ok' in cost ? cost.Ok : 0;
export const money = (cost: CostUsdTicks) => 'Ok' in cost ? `$${(cost.Ok / 1e10).toFixed(2)}` : 'Unknown';

export function providerName(id: string, profiles: ViewerProfile[]): string {
  const profile = profiles.find(p => p.id === id);
  const provider = profile?.quota_provider?.trim();
  if (provider) return provider;
  const kind = profile?.harness_kind.toLowerCase() ?? '';
  const names: Record<string, string> = {
    codex: 'OpenAI', claude: 'Anthropic', gemini: 'Google', antigravity: 'Google',
    kimi: 'Kimi', grok: 'xAI', deepseek: 'DeepSeek',
  };
  return names[kind] ?? id;
}

function allocate(values: number[], total: number): number[] {
  const sum = values.reduce((a, b) => a + b, 0);
  if (!sum) return values.map(() => 0);
  const raw = values.map(v => v / sum * total);
  const result = raw.map(Math.floor);
  const order = raw.map((v, i) => ({ i, remainder: v - result[i] }))
    .sort((a, b) => b.remainder - a.remainder || a.i - b.i);
  const missing = total - result.reduce((a, b) => a + b, 0);
  for (let i = 0; i < missing; i++) result[order[i].i]++;
  return result;
}

function mergeModels(models: ModelUsageEstimate[]): ModelUsageEstimate[] {
  const merged = new Map<string | null, ModelUsageEstimate>();
  for (const model of models) {
    let target = merged.get(model.model);
    if (!target) {
      target = { model: model.model, totals: {}, cost_usd_ticks: { Ok: 0 }, unreported_turns: 0 };
      merged.set(model.model, target);
    }
    target.cost_usd_ticks = 'Ok' in target.cost_usd_ticks && 'Ok' in model.cost_usd_ticks
      ? { Ok: target.cost_usd_ticks.Ok + model.cost_usd_ticks.Ok } : { Err: null };
    target.unreported_turns = (target.unreported_turns ?? 0) + (model.unreported_turns ?? 0);
    for (const [key, value] of Object.entries(model.totals)) {
      const old = target.totals[key] ?? { tokens: 0, reported_turns: 0 };
      target.totals[key] = { tokens: old.tokens + value.tokens, reported_turns: old.reported_turns + value.reported_turns };
    }
  }
  return [...merged.values()];
}

const roleLabels: Record<string, string> = {
  primary: 'Primary agents', orchestrator: 'Orchestrators', subtle: 'Subtle subagents',
  straightforward: 'Straightforward', oracle: 'Oracles', reviewer: 'Reviewers',
  subagent: 'Other subagents', unclassified_subagent: 'Unclassified subagents',
};

export interface ProviderRow {
  hasQuota: boolean;
  accountCount: number;
  estimatedCost: string;
  name: string;
  pace: number | null;
  partial: boolean;
  tank: number | null;
  forecast: string;
  roles: { name: string; share: number; pace: number; cost: number; unknownCost: boolean; models: ModelUsageEstimate[] }[];
}

export function providerRows(report: SessionUsageReport, profiles: ViewerProfile[], duration: number, now = Date.now() / 1000): ProviderRow[] {
  const ids = new Set([
    ...report.profiles,
    ...(report.quota_usage ?? []).map(q => q.profile),
    ...(report.quota_projections ?? []).map(q => q.profile_id),
    ...(report.profile_role_usage ?? []).flatMap(r => r.profile ? [r.profile] : []),
    ...(report.profile_usage ?? []).flatMap(r => r.profile ? [r.profile] : []),
  ]);
  const groups = new Map<string, string[]>();
  for (const id of ids) {
    const name = providerName(id, profiles);
    groups.set(name, [...(groups.get(name) ?? []), id]);
  }
  return [...groups].map(([name, members]) => {
    const usage = (report.quota_usage ?? []).filter(q => members.includes(q.profile));
    const projections = (report.quota_projections ?? []).filter(p => members.includes(p.profile_id)
      && !p.is_short_window && !p.window_label.toLowerCase().startsWith('5h'));
    const selected = members.flatMap(id => {
      const options = projections.filter(p => p.profile_id === id);
      return options.length ? [options.find(p => /^(week|weekly|7d)(\s|$)/i.test(p.window_label)) ?? options[0]] : [];
    });
    const quotaMembers = members.filter(id => usage.some(q => q.profile === id) || selected.some(p => p.profile_id === id));
    const weights = new Map(members.map(id => [id, usage.find(q => q.profile === id)?.monthly_price_usd ?? 0]));
    if (quotaMembers.some(id => !(weights.get(id)! > 0))) members.forEach(id => weights.set(id, 1));
    const measurable = usage.filter(q => q.consumed_basis_points !== null && (q.covered_seconds ?? duration) >= 60);
    const weight = (id: string) => weights.get(id) ?? 1;
    const pace = measurable.length ? Math.round(measurable.reduce((s, q) =>
      s + weight(q.profile) * q.consumed_basis_points! / 10000 * 604800 / (q.covered_seconds ?? duration) * 100, 0)
      / measurable.reduce((s, q) => s + weight(q.profile), 0)) : null;
    const partial = measurable.length > 0 && (measurable.some(q => q.partial
      || (q.covered_seconds !== undefined && q.covered_seconds < duration)) || usage.length > measurable.length);
    const totalWeight = selected.reduce((s, p) => s + weight(p.profile_id), 0);
    const tank = totalWeight ? selected.reduce((s, p) => s + p.remaining_percent * weight(p.profile_id), 0) / totalWeight : null;
    const hasQuota = quotaMembers.length > 0;
    let forecast = pace === null ? (hasQuota ? 'unavailable' : 'usage-billed') : pace === 0 ? 'idle'
      : pace < 80 ? `slack · ~${100 - pace}% unused` : pace <= 105 ? 'on pace · safe' : 'over pace';
    if (pace !== null && pace > 0 && tank !== null && selected.some(p => p.resets_at_epoch_seconds != null && p.resets_at_epoch_seconds > now)) {
      let remaining = tank;
      let last = 0;
      let dry: number | null = null;
      const balances = new Map(selected.map(p => [p.profile_id, p.remaining_percent * weight(p.profile_id) / totalWeight]));
      const events = selected.flatMap(p => {
        if (p.resets_at_epoch_seconds == null || p.resets_at_epoch_seconds <= now) return [];
        const result: { t: number; cap: number; id: string }[] = [];
        for (let t = (p.resets_at_epoch_seconds - now) / 3600; t <= 168; t += 168) {
          result.push({ t, cap: 100 * weight(p.profile_id) / totalWeight, id: p.profile_id });
        }
        return result;
      }).sort((a, b) => a.t - b.t);
      for (const event of [...events, { t: 168, cap: 0, id: '' }]) {
        let burn = (event.t - last) * pace / 168;
        if (remaining <= burn) {
          dry = last + remaining * 168 / pace;
          break;
        }
        remaining -= burn;
        while (burn > 1e-10) {
          const active = selected.filter(p => (balances.get(p.profile_id) ?? 0) > 1e-10);
          if (!active.length) break;
          const activeWeight = active.reduce((sum, p) => sum + weight(p.profile_id), 0);
          const initialBurn = burn;
          for (const p of active) {
            const old = balances.get(p.profile_id)!;
            const consumed = Math.min(old, initialBurn * weight(p.profile_id) / activeWeight);
            balances.set(p.profile_id, old - consumed);
            burn -= consumed;
          }
        }
        if (event.id) {
          const balance = balances.get(event.id)!;
          remaining += event.cap - balance;
          balances.set(event.id, event.cap);
        }
        last = event.t;
      }
      if (dry !== null) {
        const minutes = Math.ceil(dry * 60);
        forecast = minutes === 0 ? 'dry now' : minutes >= 1440
          ? `dry in ${Math.floor(minutes / 1440)}d ${Math.floor(minutes % 1440 / 60)}h`
          : `dry in ${Math.floor(minutes / 60)}h ${minutes % 60}m`;
      } else if (pace > 105) {
        forecast = 'tight · safe 7d';
      }
    }
    const profileUsage = (report.profile_usage ?? []).filter(p => p.profile && members.includes(p.profile));
    const unknown = profileUsage.some(p => 'Err' in p.cost_usd_ticks);
    const isPartial = profileUsage.some(p => p.coverage.full_turn_reports < p.coverage.recorded_turns || (p.coverage.unfinished_turns ?? 0) > 0);
    const estimatedCost = unknown ? '?' : `$${(profileUsage.reduce((sum, p) => sum + ticks(p.cost_usd_ticks), 0) / 1e10).toFixed(2)}${isPartial ? '+' : ''}`;
    const roleMap = new Map<string, { cost: number; tokens: number; unknownCost: boolean; models: ModelUsageEstimate[] }>();
    for (const role of report.profile_role_usage ?? []) {
      if (!role.profile || !members.includes(role.profile)) continue;
      const entry = roleMap.get(role.role) ?? { cost: 0, tokens: 0, unknownCost: false, models: [] };
      for (const model of role.models) {
        entry.cost += ticks(model.cost_usd_ticks);
        entry.unknownCost ||= 'Err' in model.cost_usd_ticks || (model.unreported_turns ?? 0) > 0;
      }
      entry.models.push(...role.models);
      entry.tokens += role.models.reduce((s, m) => s + (m.totals.total_tokens?.tokens ?? 0), 0);
      roleMap.set(role.role, entry);
    }
    const entries = [...roleMap];
    const useCost = entries.some(([, r]) => r.cost > 0);
    const vals = entries.map(([, r]) => useCost ? r.cost : r.tokens);
    const shares = allocate(vals, 100);
    const paces = allocate(vals, pace ?? 0);
    const roles = entries.map(([role, r], i) => ({
      name: roleLabels[role] ?? role.replaceAll('_', ' '), share: shares[i], pace: paces[i],
      cost: r.cost, unknownCost: r.unknownCost, models: mergeModels(r.models),
    }));
    if (!vals.some(v => v > 0) && pace && pace > 0) {
      roles.splice(0, roles.length, { name: 'External / untracked', share: 100, pace, cost: 0, unknownCost: false, models: [] });
    }
    return { name, pace, partial, tank, forecast, roles, hasQuota, accountCount: quotaMembers.length, estimatedCost };
  });
}
