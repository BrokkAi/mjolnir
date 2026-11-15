export const meta = {
  name: 'bifrost-tool-coverage-probe2',
  description: 'Ask Bifrost 0.15.2 query steps and flow policies the census questions that navigation tools did not answer',
  phases: [{ title: 'Probe', detail: 'one Opus/high agent per record, strictly one after another' }],
}
// args: {spec: '<path to toolcov/SPEC-probe2.md>', ids: [record ids]}
const ATTEMPT = {
  type: 'object',
  properties: {
    surface: { type: 'string' }, input: { type: 'string' },
    outcome: { type: 'string', enum: ['works', 'incomplete', 'unsupported', 'wrong', 'error'] },
    result_count: { type: ['integer', 'null'] }, rank: { type: ['integer', 'null'] },
    status: { type: ['string', 'null'] }, seconds: { type: ['number', 'null'] }, diagnostic: { type: ['string', 'null'] },
  },
  required: ['surface', 'input', 'outcome', 'result_count', 'rank', 'status', 'seconds', 'diagnostic'],
}
const SCHEMA = {
  type: 'object',
  properties: {
    id: { type: 'string' },
    attempts: { type: 'array', items: ATTEMPT },
    best_outcome: { type: 'string', enum: ['works', 'incomplete', 'unsupported', 'wrong'] },
    capability: { type: 'string', enum: ['A1', 'A3', 'A4', 'A6', 'B3', 'B4', 'B6', 'github-hop'] },
    note: { type: 'string' },
  },
  required: ['id', 'attempts', 'best_outcome', 'capability', 'note'],
}
phase('Probe')
// Serial: all worktrees share one Bifrost cache, whose build lock blocks concurrent processes.
const records = []
for (const id of args.ids) {
  const r = await agent(
    `Read ${args.spec} and follow it exactly. Your record: ${id}. Return one object for it.`,
    { label: id, phase: 'Probe', schema: SCHEMA, model: 'opus', effort: 'high' })
  if (r) records.push({ ...r, id })
}
return { records, missing: args.ids.filter(id => !records.some(r => r.id === id)) }
