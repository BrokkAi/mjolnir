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
    layout = '\nmain{max-width:1000px;padding:12px 16px 16px}.panels{display:block}\n'
    page = f'''<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Low+ review findings</title><style>{CSS}{layout}</style></head>
<body><main>
{colorbar_svg(payload['vmax'])}
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
