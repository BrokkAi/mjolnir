export const meta = {
  name: 'bifrost-tool-coverage',
  description: 'Check whether Bifrost 0.15.2 produces each census relation from a diff-only query (A/B/C)',
  phases: [{ title: 'Probe', detail: 'one Sonnet agent per group of about 3 records' }],
}
// args: {spec: '<path to toolcov/SPEC.md>', groups: [[record ids], ...]}
const CALL = {
  type: 'object',
  properties: {
    tool: { type: 'string' }, args: { type: 'string' },
    anchor_found: { type: 'boolean' }, result_count: { type: ['integer', 'null'] },
    rank: { type: ['integer', 'null'] }, status: { type: ['string', 'null'] },
    seconds: { type: ['number', 'null'] }, error: { type: ['string', 'null'] },
  },
  required: ['tool', 'args', 'anchor_found', 'result_count', 'rank', 'status', 'seconds', 'error'],
}
const SCHEMA = {
  type: 'object',
  properties: {
    records: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          id: { type: 'string' },
          decisive_anchor: { type: 'string' },
          diff_only_query: { type: 'string' },
          calls: { type: 'array', items: CALL },
          anchor_found: { type: 'boolean' },
          class: { type: 'string', enum: ['A', 'A0', 'B', 'C'] },
          missing: { type: ['string', 'null'] },
          note: { type: 'string' },
          index_seconds: { type: ['number', 'null'] },
        },
        required: ['id', 'decisive_anchor', 'diff_only_query', 'calls', 'anchor_found', 'class', 'missing', 'note', 'index_seconds'],
      },
    },
  },
  required: ['records'],
}
phase('Probe')
const results = await parallel(args.groups.map((ids, n) => () => agent(
  `Read ${args.spec} and follow it exactly. Your records: ${ids.join(', ')}. Return one entry per record.`,
  { label: `group ${n}`, phase: 'Probe', schema: SCHEMA, model: 'sonnet', effort: 'high' },
).then(r => (r?.records || []).filter(x => ids.includes(x.id)))))
const records = results.filter(Boolean).flat()
return { records, missing: args.groups.flat().filter(id => !records.some(r => r.id === id)) }
