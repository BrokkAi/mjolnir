"""Reprice verify10 combinations and reconstruct exact cost-frontier membership."""
import collections
import csv
from fractions import Fraction
import hashlib
import itertools
import json
from pathlib import Path
import re

HERE = Path(__file__).resolve().parent
ROOT = Path('/mnt/optane/review-arms-20261113')
SOURCE = HERE.parent
FILES = {'all': 'pareto.csv', 'medium-high': 'pareto-sev-high-medium.csv', 'high': 'pareto-sev-high.csv'}
LABELS = {'gpt-6.1-terra': 'Terra', 'gpt-6.1-sol': 'Sol', 'muse-ember-1.0': 'Ember',
          'greptile': 'Greptile', 'gemini-4-hydrogen': 'Gemini', 'kimi-code/k3.1-pro': 'Kimi', 'deepseek-flash': 'DSF'}


def read_csv(path):
    with path.open(newline='') as source:
        return list(csv.DictReader(source))


def members(option):
    result = collections.Counter()
    for part in option.split(' + '):
        match = re.fullmatch(r'(?:(\d+)× )?(.+)', part)
        assert match
        result[match[2]] += int(match[1] or 1)
    return result


def fraction(value):
    return Fraction(str(value))


def nondominated(rows, costs, findings):
    eligible = [row['option'] for row in rows if 'Greptile/' not in row['option']]
    return {o for o in eligible if not any(costs[p] <= costs[o] and findings[p] >= findings[o]
            and (costs[p] < costs[o] or findings[p] > findings[o]) for p in eligible if p != o)}


def main():
    sessions_path = ROOT / 'verify10/sessions.csv'
    group_path = ROOT / 'verify10/groups.csv'
    links_path = ROOT / 'verify10/session_groups.csv'
    price_path = SOURCE / 'additional-model-prices.json'
    prices = json.loads(price_path.read_text())['options']
    sessions = read_csv(sessions_path)
    assert len(sessions) == 1796
    per_diff = collections.defaultdict(lambda: collections.defaultdict(list))
    cost_pools = collections.defaultdict(lambda: collections.defaultdict(list))
    for row in sessions:
        config = LABELS.get(row['model'], row['model']) + '/' + row['effort']
        per_diff[row['bug']][config].append(row['session'])
        cost_pools[config][row['bug']].append(fraction(row['cost_usd']) if row['cost_usd'] else None)
    assert len(per_diff) == 70
    costs = {}
    for config, diffs in cost_pools.items():
        if config in prices:
            price = prices[config]
            assert price['diffs'] == price['reviews'] == len(diffs) == 70
            costs[config] = (fraction(price['cost_low']), fraction(price['cost_high']))
        elif len(diffs) == 70 and all(p is not None for pool in diffs.values() for p in pool):
            # A mean within each diff, then across diffs: Terra has 3 or 4 samples.
            mean = sum(sum(pool, Fraction()) / len(pool) for pool in diffs.values()) / 70
            costs[config] = (mean, mean)
    groups = {(row['bug'], row['group']): row for row in read_csv(group_path)}
    found = collections.defaultdict(set)
    for row in read_csv(links_path):
        key = row['bug'], row['group']
        if groups[key]['verdict'] == 'confirmed':
            found[row['session']].add(key)
    rows_by_tier = {tier: read_csv(SOURCE / name) for tier, name in FILES.items()}
    option_set = {row['option'] for row in rows_by_tier['all']}
    assert len(option_set) == 571
    assert all({row['option'] for row in rows} == option_set for rows in rows_by_tier.values())
    exact = {tier: {} for tier in FILES}
    low_cost, high_cost = {}, {}
    for option in sorted(option_set):
        counts = members(option)
        assert 1 <= sum(counts.values()) <= 3 and set(counts) <= set(costs)
        low_cost[option] = sum((n * costs[c][0] for c, n in counts.items()), Fraction())
        high_cost[option] = sum((n * costs[c][1] for c, n in counts.items()), Fraction())
        totals = dict.fromkeys(FILES, Fraction())
        for bug, available in per_diff.items():
            pools = [list(itertools.combinations(available[c], n)) for c, n in sorted(counts.items())]
            assert all(pools), (option, bug)
            hits = collections.Counter()
            assignments = 0
            for parts in itertools.product(*pools):
                union = set().union(*(found[sid] for part in parts for sid in part))
                hits['all'] += len(union)
                hits['medium-high'] += sum(groups[g]['severity'] in {'medium', 'high'} for g in union)
                hits['high'] += sum(groups[g]['severity'] == 'high' for g in union)
                assignments += 1
            for tier in FILES:
                totals[tier] += Fraction(hits[tier], assignments)
        for tier in FILES:
            exact[tier][option] = totals[tier]
    metadata = dict(round='verify10', rows=571, non_greptile_options=386, greptile_solo=2,
                    greptile_combinations_excluded=183, priced_configurations=len(costs),
                    price_basis='Same verified standard API estimates as the solo chart; Ember upper endpoint.',
                    notes=["Hydrogen costs assume its saved unspecified-scope reports cover the full turn.",
                           "Ember costs use the conservative over-200k tier; ranges show all-under vs all-over bounds.",
                           "Prices cover reviewer turns, excluding setup, judging and subscriptions."],
                    sources={}, frontiers={}, config_cost_bounds={c: [str(a), str(b)] for c, (a, b) in costs.items()})
    for path in [sessions_path, group_path, links_path, price_path, *[SOURCE / n for n in FILES.values()]]:
        metadata['sources'][str(path)] = hashlib.sha256(path.read_bytes()).hexdigest()
    for tier, filename in FILES.items():
        rows = rows_by_tier[tier]
        on = nondominated(rows, high_cost, exact[tier])
        metadata['frontiers'][tier] = sorted(on)
        for row in rows:
            option = row['option']
            assert abs(float(exact[tier][option]) - float(row['findings'])) <= .0051
            if row['cost_per_diff_usd']:
                assert abs(float(high_cost[option]) - float(row['cost_per_diff_usd'])) <= .00051
            row['cost_per_diff_usd'] = str(float(high_cost[option]))
            row['cost_lower_usd'] = str(float(low_cost[option]))
            row['cost_exact'] = str(high_cost[option])
            row['findings_exact'] = str(exact[tier][option])
            row['cost_frontier'] = str(option in on)
        with (HERE / filename).open('w', newline='') as output:
            writer = csv.DictWriter(output, fieldnames=list(rows[0]), lineterminator='\n')
            writer.writeheader()
            writer.writerows(rows)
        if tier in {'all', 'medium-high'}:
            budget_rows = [dict(row, cost_per_diff_usd=f"{float(high_cost[row['option']]):.3f}") for row in rows]
            with (HERE / ('budget-' + filename)).open('w', newline='') as output:
                writer = csv.DictWriter(output, fieldnames=list(budget_rows[0]), lineterminator='\n')
                writer.writeheader(); writer.writerows(budget_rows)
        print(tier, len(rows), 'rows;', len(on), 'cost frontier options')
    (HERE / 'combo-pricing.json').write_text(json.dumps(metadata, indent=2) + '\n')


if __name__ == '__main__':
    main()
