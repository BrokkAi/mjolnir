export const meta = {
  name: 'missing-fact-labels',
  description: 'Opus labels the one non-diff fact each review-arms defect needed, and its kind',
  phases: [{ title: 'Label', detail: 'one Opus/high labeler per batch of about 5 commits' }],
}
// args: {dir: '<REVIEW_ARMS_DIR>/label-facts', batches: [[item IDs of batch-0.json], ...]}
const DIR = args.dir
const ROOT = DIR.replace(/\/label-facts\/?$/, '')
const NULLSTR = { type: ['string', 'null'] }
const SCHEMA = {
  type: 'object',
  properties: {
    labels: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          id: { type: 'string' },
          defect_check: { type: 'string' },
          fact: { type: 'string' },
          kind: { type: 'string', enum: ['affected callers', 'path', 'value flow', 'none'] },
          none_reason: { type: ['string', 'null'], enum: ['in_diff', 'intent', 'external', null] },
          alt_kind: { type: ['string', 'null'], enum: ['affected callers', 'path', 'value flow', 'none', null] },
          query: NULLSTR,
          other_facts: { type: 'array', items: { type: 'string' } },
          evidence: { type: 'string' },
          confidence: { type: 'string', enum: ['high', 'medium', 'low'] },
        },
        required: ['id', 'defect_check', 'fact', 'kind', 'none_reason', 'alt_kind', 'query', 'other_facts', 'evidence', 'confidence'],
      },
    },
  },
  required: ['labels'],
}
const prompt = (n, ids) => `Read ${DIR}/SPEC.md and follow it exactly, with one change: do not write label-out.json; return the labels through the StructuredOutput tool instead.

Your batch file: ${DIR}/batch-${n}.json
${ids ? `Label only these items from it: ${ids.join(', ')}.` : 'Label every item in it.'}

Use the git repository in your current working directory (it has every commit named in the batch). Read only SPEC.md, your batch file, and that repository's history and code. Do not open .agents/evals/ or anything else under ${ROOT}: other files there hold other labels and model results. Do not build, run tests, or edit files.`

phase('Label')
const results = await parallel(args.batches.map((ids, n) => async () => {
  const opts = { label: `batch ${n}`, phase: 'Label', schema: SCHEMA, model: 'opus', effort: 'high' }
  const first = await agent(prompt(n), opts)
  const got = new Map((first?.labels || []).filter(l => ids.includes(l.id)).map(l => [l.id, l]))
  const missing = ids.filter(id => !got.has(id))
  if (missing.length) {
    log(`batch ${n}: retrying ${missing.length} missing item(s): ${missing.join(', ')}`)
    const again = await agent(prompt(n, missing), { ...opts, label: `batch ${n} retry` })
    for (const l of again?.labels || []) if (missing.includes(l.id)) got.set(l.id, l)
  }
  return { batch: n, labels: ids.map(id => got.get(id)).filter(Boolean), missing: ids.filter(id => !got.has(id)) }
}))
const labels = results.filter(Boolean).flatMap(r => r.labels)
const missing = args.batches.flat().filter(id => !labels.some(l => l.id === id))
return { labels, missing }
