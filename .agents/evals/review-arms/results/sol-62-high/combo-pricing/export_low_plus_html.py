"""Export only the all-impact budget map, without surrounding commentary."""
import copy
import json
from pathlib import Path
from plot_budget_maps import CSS, JS, colorbar_svg, svg_panel

HERE = Path(__file__).resolve().parent
STEM = 'review-opus-high-budget-low-plus'


def export():
    original = json.loads((HERE / 'review-opus-high-budget-data.json').read_text())
    panel = copy.deepcopy(next(p for p in original['panels'] if p['id'] == 'all'))
    panel['title'] = 'Low+ findings'
    payload = dict(original, panels=[panel], sources={name: digest for name, digest in original['sources'].items()
                                                   if name == 'budget-pareto.csv'})
    payload['vmax'] = panel['max_f']
    data = json.dumps(payload, separators=(',', ':'), ensure_ascii=True).replace('</', '<\\/')
    layout = '\nmain{max-width:1000px;padding:12px 16px 16px}.panels{display:block}\n.marker-key{width:min(520px,100%);display:block;margin:8px 0}\n'
    scale = colorbar_svg(payload['vmax']).replace('viewBox="0 0 440 66"', 'viewBox="0 0 440 84"').replace(
        'Expected confirmed findings (best setup within both limits)',
        'Expected Low+ confirmed findings across 70 diffs</text>'
        '<text x="220" y="78" text-anchor="middle" class="al">(best setup within both limits)')
    key = '\n'.join([
        '<svg viewBox="0 0 520 44" class="marker-key" role="img" aria-label="Chart marker key">',
        '<circle cx="8" cy="10" r="3.6" fill="#fff" stroke="#0b0b0b"/>',
        '<text x="22" y="14" class="al">Best setup within some budget</text>',
        '<path d="M8 25L14 31L8 37L2 31Z" fill="#ddd" stroke="#0b0b0b"/>',
        '<text x="22" y="35" class="al">Greptile solo reference (excluded from optimization)</text>',
        '</svg>',
    ])
    page = f'''<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Low+ review findings</title><style>{CSS}{layout}</style></head>
<body><main>
{scale}
{key}
<div class="panels"><figure>{svg_panel(panel, payload['domain'], payload['vmax'])}</figure></div>
<div id="tip" role="status"></div>
<script id="budget-data" type="application/json">{data}</script>
<script id="app">{JS}</script>
</main></body></html>
'''
    target = HERE / (STEM + '.html')
    target.write_text(page, encoding='utf-8')
    print(target)


if __name__ == '__main__':
    export()
