"""Price the retained Hydrogen, Kimi and Ember solo reviews without altering raw data."""
import collections
import csv
import hashlib
import json
from pathlib import Path
from statistics import fmean

ROOT = Path('/mnt/optane/review-arms-20261113')
CAMPAIGN = ROOT / 'sol-62-high'
RATES = {
    'gemini-4-hydrogen': dict(input=1.5, cached_input=.075, output=7.5,
                            source='https://aistudio.google.com/docs/pricing#gemini-4-hydrogen'),
    'kimi-code/k3.1-pro': dict(input=2., cached_input=.2, cache_write=2., output=10.,
                             source='https://platform.kimi.ai/',
                             caching_source='https://platform.kimi.ai/docs/guide/context-caching'),
    'muse-ember-1.0': dict(input=.1, cached_input=.004, output=.5,
                         above_200k_multiplier=3., source='https://dev.meta.ai/models/muse-ember'),
}
OPTIONS = {'gemini-4-hydrogen': 'Gemini', 'kimi-code/k3.1-pro': 'Kimi', 'muse-ember-1.0': 'Ember'}
manifest_path = CAMPAIGN / 'baseline-sessions.json'
manifest = json.loads(manifest_path.read_text())
scored = {row['session']: row for row in csv.DictReader((ROOT / 'verify10/sessions.csv').open())}
selected = [row for row in manifest if row['model'] in RATES]
assert len(selected) == 280
by_config = collections.defaultdict(list)
hashes = []
for row in selected:
    path = ROOT / Path(row['output']).with_name('usage.json')
    hashes.append(hashlib.sha256(path.read_bytes()).hexdigest() + '  ' + str(path.relative_to(ROOT)))
    saved = json.loads(path.read_text())
    assert saved['session_id'] == row['child'] and row['child'] in scored
    assert (scored[row['child']]['model'], scored[row['child']]['effort']) == (row['model'], row['effort'])
    assert len(saved['turns']) == 1
    turn = saved['turns'][0]
    assert turn['outcome'] == {'kind': 'completed', 'stop_reason': 'EndTurn'}
    assert saved['turn_selections'][turn['command_id']]['model'] == row['model']
    u = turn['usage']
    i, o, cr, cw, total = [u[k] for k in ['input_tokens', 'output_tokens', 'cached_read_tokens', 'cached_write_tokens', 'total_tokens']]
    assert all(isinstance(n, int) and n >= 0 for n in [i, o, cr, cw, total])
    assert 0 <= u.get('thought_tokens', 0) <= o
    # Saved Hydrogen reports predate the adapter's scope normalization. The chart
    # explicitly assumes these prompt-response counters cover the completed turn.
    if row['model'] == 'gemini-4-hydrogen':
        assert u['scope'] in {'unspecified', 'turn'} and i + o + cr + cw == total and cw == 0
        fresh = i
    else:
        assert u['scope'] == 'turn' and i + o == total and i >= cr + cw
        fresh = i - cr - cw
    rates = RATES[row['model']]
    if row['model'] == 'muse-ember-1.0':
        assert cw == 0  # No published separate write tariff is needed for these captures.
    cost = (fresh * rates['input'] + cr * rates['cached_input'] +
            cw * rates.get('cache_write', 0) + o * rates['output']) / 1_000_000
    by_config[row['model'], row['effort']].append(dict(bug=row['bug'], cost=cost, scope=u['scope'],
                                                      fresh_input=fresh, output=o, cached_input=cr, cache_write=cw))
result = dict(round='verify10', rates_verified_utc='2026-11-25', units='USD per million tokens', rates=RATES,
              interpretation='Estimated standard API equivalent, not subscription spend. Output already includes thinking.',
              source_manifest_sha256=hashlib.sha256(manifest_path.read_bytes()).hexdigest(), options={})
for (model, effort), rows in sorted(by_config.items()):
    assert len(rows) == len({row['bug'] for row in rows}) == 70
    low = fmean(row['cost'] for row in rows)
    high = low * RATES[model].get('above_200k_multiplier', 1.)
    note = ('Context-tier bound: all tokens below 200k vs all above; marker uses conservative upper endpoint. '
            'Per-request context sizes were not retained.' if model == 'muse-ember-1.0' else
            'Assumes the saved unspecified-scope prompt-response counters cover the full turn.' if model == 'gemini-4-hydrogen' else
            'Complete turn counters; five-minute API cache-write tariff, mutually exclusive input categories.')
    result['options'][OPTIONS[model] + '/' + effort] = dict(
        reviews=70, diffs=70, cost_low=low, cost_high=high, cost=high,
        usage_scope_counts=dict(collections.Counter(row['scope'] for row in rows)),
        mean_tokens={key: fmean(row[key] for row in rows) for key in ['fresh_input', 'output', 'cached_input', 'cache_write']},
        cost_note=note, pricing_source=RATES[model]['source'])
(CAMPAIGN / 'additional-model-prices.json').write_text(json.dumps(result, indent=2) + '\n')
(CAMPAIGN / 'additional-pricing-inputs.sha256').write_text('\n'.join(sorted(hashes)) + '\n')
print(json.dumps(result['options'], indent=2))
