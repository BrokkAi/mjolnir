export const meta = {
  name: 'missing-fact-labels-pass2',
  description: 'Opus re-labels defects judged visible in the diff: what fact could a static analysis tool have surfaced',
  phases: [{ title: 'Label', detail: 'one Opus/high labeler per item' }],
}
// args: {dir: '<REVIEW_ARMS_DIR>/label-facts/pass2', ids: [item IDs]}
const DIR = args.dir
const ROOT = DIR.replace(/\/label-facts\/pass2\/?$/, '')
const KINDS = ['affected callers', 'path', 'value flow', 'none']
const SCHEMA = {
  type: 'object',
  properties: {
    id: { type: 'string' },
    fact: { type: 'string' },
    kind: { type: 'string', enum: KINDS },
    none_reason: { type: ['string', 'null'], enum: ['intent', 'logic', 'external', null] },
    alt_kind: { type: ['string', 'null'], enum: [...KINDS, null] },
    query: { type: ['string', 'null'] },
    tool_output: { type: ['string', 'null'] },
    specificity: { type: ['string', 'null'], enum: ['high', 'medium', 'low', null] },
    specificity_reason: { type: ['string', 'null'] },
    agrees_with_first_pass: { type: 'boolean' },
    evidence: { type: 'string' },
    confidence: { type: 'string', enum: ['high', 'medium', 'low'] },
  },
  required: ['id', 'fact', 'kind', 'none_reason', 'alt_kind', 'query', 'tool_output', 'specificity',
    'specificity_reason', 'agrees_with_first_pass', 'evidence', 'confidence'],
}
phase('Label')
const labels = await parallel(args.ids.map(id => () => agent(
  `Read ${DIR}/SPEC.md and follow it exactly.

Your item: ${DIR}/item-${id}.json (id "${id}").

Use the git repository in your current working directory. Read only SPEC.md, your item file, and that repository's history and code. Do not open .agents/evals/ or anything else under ${ROOT}: other files there hold other labels and model results. Do not build, run tests, or edit files.`,
  { label: id, phase: 'Label', schema: SCHEMA, model: 'opus', effort: 'high' },
).then(r => r && { ...r, id })))
const got = labels.filter(Boolean)
return { labels: got, missing: args.ids.filter(id => !got.some(l => l.id === id)) }
