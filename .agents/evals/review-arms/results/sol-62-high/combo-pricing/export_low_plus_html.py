"""Export only the all-impact budget map, without surrounding commentary."""
import argparse
import copy
import json
from io import StringIO
from pathlib import Path

import matplotlib.pyplot as plt
from plot_budget_maps import (
    BG, CSS, JS, PAD, PNG_DPI, colorbar_svg, draw, insert_svg_metadata, svg_panel,
)

HERE = Path(__file__).resolve().parent
STEM = 'review-opus-high-budget-low-plus'


def export(*, no_greptile=False, linear_x=False):
    original = json.loads((HERE / 'review-opus-high-budget-data.json').read_text())
    panel = copy.deepcopy(next(p for p in original['panels'] if p['id'] == 'all'))
    panel['title'] = 'Low+ findings'
    payload = dict(original, panels=[panel], sources={name: digest for name, digest in original['sources'].items()
                                                   if name == 'budget-pareto.csv'})
    payload['domain'] = copy.deepcopy(original['domain'])
    if no_greptile:
        panel['counts']['greptile_rows_omitted'] += len(panel['greptile'])
        panel['counts']['greptile_solo'] = 0
        panel['greptile'] = []
        payload['domain']['c'] = [min(o['c'] for o in panel['options']) / PAD,
                                  max(o['c'] for o in panel['options']) * PAD]
        payload['domain']['t'] = [min(o['t'] for o in panel['options']) / PAD,
                                  max(o['t'] for o in panel['options']) * PAD]
    if linear_x:
        # Every score-changing threshold remains visible; the final region extends indefinitely.
        last = max([*panel['ts'], *(g['t'] for g in panel['greptile'])])
        payload['domain']['t'] = [0, last * 1.08]
    payload['x_scale'] = 'linear' if linear_x else 'log'
    payload['vmax'] = panel['max_f']
    suffix = ('-linear' if linear_x else '') + ('-no-greptile' if no_greptile else '')
    target = HERE / (STEM + suffix + '.html')
    if linear_x or no_greptile:
        geometry = {}
        fig = draw(panel, payload['domain'], payload['vmax'], geometry, linear_x=linear_x)
        fig.savefig(target.with_suffix('.png'), dpi=PNG_DPI, facecolor=BG)
        buffer = StringIO()
        fig.savefig(buffer, format='svg', facecolor=BG, metadata={'Date': None})
        panel['png'] = geometry['all']
        svg = insert_svg_metadata(buffer.getvalue(), {'schema': 1, 'domain': payload['domain'],
            'vmax': payload['vmax'], 'x_scale': payload['x_scale'], 'panel': panel})
        target.with_suffix('.svg').write_text('\n'.join(line.rstrip() for line in svg.splitlines()) + '\n')
        plt.close(fig)
    data = json.dumps(payload, separators=(',', ':'), ensure_ascii=True).replace('</', '<\\/')
    layout = '\nmain{max-width:1000px;padding:12px 16px 16px}.panels{display:block}\n.marker-key{width:min(520px,100%);display:block;margin:8px 0}\n'
    scale = colorbar_svg(payload['vmax']).replace('viewBox="0 0 440 66"', 'viewBox="0 0 440 84"').replace(
        'Expected confirmed findings (best setup within both limits)',
        'Expected Low+ confirmed findings across 70 diffs</text>'
        '<text x="220" y="78" text-anchor="middle" class="al">(best setup within both limits)')
    key = '\n'.join([
        f'<svg viewBox="0 0 520 {22 if no_greptile else 44}" class="marker-key" role="img" aria-label="Chart marker key">',
        '<circle cx="8" cy="10" r="3.6" fill="#fff" stroke="#0b0b0b"/>',
        '<text x="22" y="14" class="al">Best setup within some budget</text>',
        *([] if no_greptile else [
            '<path d="M8 25L14 31L8 37L2 31Z" fill="#ddd" stroke="#0b0b0b"/>',
            '<text x="22" y="35" class="al">Greptile solo reference (excluded from optimization)</text>',
        ]),
        '</svg>',
    ])
    page = f'''<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Low+ review findings</title><style>{CSS}{layout}</style></head>
<body><main>
{scale}
{key}
<div class="panels"><figure>{svg_panel(panel, payload['domain'], payload['vmax'], linear_x=linear_x)}</figure></div>
<div id="tip" role="status"></div>
<script id="budget-data" type="application/json">{data}</script>
<script id="app">{JS}</script>
</main></body></html>
'''
    target.write_text(page, encoding='utf-8')
    print(target)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--no-greptile', action='store_true')
    parser.add_argument('--linear-x', action='store_true')
    args = parser.parse_args()
    export(no_greptile=args.no_greptile, linear_x=args.linear_x)
