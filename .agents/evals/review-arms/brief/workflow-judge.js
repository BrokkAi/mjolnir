export const meta = {
  name: 'brief-judge',
  description: 'Two independent Opus/high judges label every reviewer output caught, partial or missed against its known defect',
  phases: [{ title: 'Judge', detail: 'two judges per batch, retrying any output a judge left out' }],
}
// args: {dir: '<REVIEW_ARMS_DIR>/brief-judge', batches: [{n, ids: [output ids]}]}  (brief_score.py build)
const DIR = args.dir
const ROOT = DIR.replace(/\/brief-judge\/?$/, '')
const SCHEMA = {
  type: 'object',
  properties: {
    verdicts: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          output_id: { type: 'string' },
          bug: { type: 'string' },
          label: { type: 'string', enum: ['caught', 'partial', 'missed'] },
          matching_finding: { type: ['string', 'null'] },
          rationale: { type: 'string' },
          finding_count: { type: 'integer' },
          confidence: { type: 'string', enum: ['high', 'medium', 'low'] },
        },
        required: ['output_id', 'bug', 'label', 'matching_finding', 'rationale', 'finding_count', 'confidence'],
      },
    },
  },
  required: ['verdicts'],
}
const prompt = (n, only) => `Read ${DIR}/SPEC.md and follow it exactly, with one change: return the verdicts through the StructuredOutput tool instead of writing verdicts.jsonl.

Your batch: ${DIR}/batch-${n}.json
${only ? `Judge only these outputs from it: ${only.join(', ')}.` : 'Judge every output in it.'}

The mjolnir checkout with full history is your current working directory. Read only SPEC.md, your batch file, and that repository. Do not open .agents/evals/ or anything else under ${ROOT}: other files there reveal which model or setup wrote which output. Do not build, run tests, or edit files.`

phase('Judge')
const runs = await parallel(args.batches.flatMap(b => ['A', 'B'].map(j => async () => {
  const opts = { label: `batch ${b.n} judge ${j}`, phase: 'Judge', schema: SCHEMA, model: 'opus', effort: 'high' }
  const got = new Map()
  for (const v of (await agent(prompt(b.n), opts))?.verdicts || []) if (b.ids.includes(v.output_id)) got.set(v.output_id, v)
  const missing = b.ids.filter(id => !got.has(id))
  if (missing.length) {
    log(`batch ${b.n} judge ${j}: retrying ${missing.length} output(s)`)
    for (const v of (await agent(prompt(b.n, missing), { ...opts, label: `batch ${b.n} judge ${j} retry` }))?.verdicts || [])
      if (missing.includes(v.output_id)) got.set(v.output_id, v)
  }
  return { batch: b.n, judge: j, verdicts: [...got.values()].map(v => ({ ...v, judge: j })) }
})))
const verdicts = runs.filter(Boolean).flatMap(r => r.verdicts)
const missing = args.batches.flatMap(b => b.ids.flatMap(id => ['A', 'B']
  .filter(j => !verdicts.some(v => v.output_id === id && v.judge === j)).map(j => `${id}/${j}`)))
return { verdicts, missing }
