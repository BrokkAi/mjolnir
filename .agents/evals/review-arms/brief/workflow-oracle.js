export const meta = {
  name: 'oracle-briefs',
  description: 'Insert each decisive relation into its blind brief, then leak-check every brief with the known-bug judge',
  phases: [
    { title: 'Edit', detail: 'Opus/high oracle editor for primary and secondary diffs' },
    { title: 'Leak', detail: 'Opus/high judge reads each brief as a review output; rewrite once if caught or partial' },
  ],
}
// args: {dir: '<REVIEW_ARMS_DIR>/briefs/oracle-work', diffs: [{id, group: 'P'|'S'|'C', blind_sha256}], blind_only?: true}
// blind_only: leak-check each diff's blind brief without the editor (stage B; brief_oracle.py blind-collect).
// Written by brief_oracle.py build: ORACLE.md, WRITER.md, JUDGE.md, item-<id>.json, record-<id>.json.
const DIR = args.dir
const ROOT = DIR.replace(/\/briefs\/oracle-work\/?$/, '')
const OPTS = { model: 'opus', effort: 'high' }
const FENCE = `Read only the files named here and the repository in your current working directory (git history and code). Do not open .agents/evals/ or anything else under ${ROOT}. Do not build, run tests, edit files, or use the network.`
const EDIT = {
  type: 'object',
  properties: {
    id: { type: 'string' },
    contains: { type: 'string', enum: ['yes', 'partial', 'no'] },
    matching_item: { type: ['string', 'null'] },
    inserted_item: { type: ['string', 'null'] },
    replaced_item: { type: ['string', 'null'] },
    oracle_brief: { type: 'string' },
    rationale: { type: 'string' },
  },
  required: ['id', 'contains', 'matching_item', 'inserted_item', 'replaced_item', 'oracle_brief', 'rationale'],
}
const VERDICT = {
  type: 'object',
  properties: {
    label: { type: 'string', enum: ['caught', 'partial', 'missed'] },
    matching_finding: { type: ['string', 'null'] },
    rationale: { type: 'string' },
    confidence: { type: 'string', enum: ['high', 'medium', 'low'] },
  },
  required: ['label', 'matching_finding', 'rationale', 'confidence'],
}
const REWRITE = {
  type: 'object',
  properties: { brief: { type: 'string' }, changes: { type: 'string' } },
  required: ['brief', 'changes'],
}

const judge = (d, brief, round) => agent(
  `Read ${DIR}/JUDGE.md. It describes judging code-review outputs against a known bug. Apply its Decision section to one output: the text below, which you judge as if it were a review output. Return your verdict through the StructuredOutput tool instead of writing verdicts.jsonl.

The known bug's record: ${DIR}/record-${d.id}.json

The output to judge:

<output>
${brief}
</output>

${FENCE}`,
  { ...OPTS, label: `leak ${d.id}${round ? ' (after rewrite)' : ''}`, phase: 'Leak', schema: VERDICT })

const results = await pipeline(
  args.diffs,
  async d => {
    // Control briefs are the blind brief; the editor never sees them. blind_only checks every
    // diff's blind brief that way.
    if (d.group === 'C' || args.blind_only) return { edit: null, brief: null }
    const edit = await agent(
      `Read ${DIR}/ORACLE.md and follow it exactly. The brief format and rules are in ${DIR}/WRITER.md. Your item file: ${DIR}/item-${d.id}.json (id "${d.id}").

${FENCE}`,
      { ...OPTS, label: `edit ${d.id}`, phase: 'Edit', schema: EDIT })
    if (!edit) throw new Error(`no edit for ${d.id}`)
    return { edit, brief: edit.oracle_brief }
  },
  async ({ edit, brief }, d) => {
    const first = brief
      ? await judge(d, brief, 0)
      : await agent(
        `Read ${DIR}/JUDGE.md. It describes judging code-review outputs against a known bug. Apply its Decision section to one output: the \`blind_brief\` field of ${DIR}/item-${d.id}.json, which you judge as if it were a review output. Return your verdict through the StructuredOutput tool instead of writing verdicts.jsonl.

The known bug's record: ${DIR}/record-${d.id}.json

${FENCE}`,
        { ...OPTS, label: `leak ${d.id}`, phase: 'Leak', schema: VERDICT })
    if (!first) throw new Error(`no leak verdict for ${d.id}`)
    const leak = [first]
    let final = brief
    if (first.label !== 'missed') {
      const rw = await agent(
        `A brief of relations for a code reviewer (rules in ${DIR}/WRITER.md) was judged to point at a known defect: "${first.rationale}". Rewrite it so that it states only relations between code locations, with no judgment, consequence or hint of what goes wrong. Keep every relation and every file:line anchor, including the one the judge matched: remove only wording that goes beyond the relation. A clause that says what code does or does not do, such as which paths a script removes and which it leaves, or which function a call reaches, is itself a relation: keep it, even if it points at the defect. Remove only evaluative or consequence wording (for example "only" used as emphasis, "so ...", "which means ..."). Change nothing else.

${brief ? `The brief:\n\n<brief>\n${brief}\n</brief>` : `The brief is the \`blind_brief\` field of ${DIR}/item-${d.id}.json.`}

${FENCE}`,
        { ...OPTS, label: `rewrite ${d.id}`, phase: 'Leak', schema: REWRITE })
      if (!rw) throw new Error(`no rewrite for ${d.id}`)
      final = rw.brief
      const again = await judge(d, final, 1)
      if (!again) throw new Error(`no second leak verdict for ${d.id}`)
      leak.push({ ...again, rewrite_changes: rw.changes })
    }
    // final_brief is null when a control brief needed no rewrite: it is the blind brief.
    // judged_blind_sha256 binds a verdict on the item file's blind brief to that text.
    return { id: d.id, group: d.group, edit, leak, final_brief: final,
             judged_blind_sha256: brief ? null : (d.blind_sha256 ?? null) }
  },
)
const done = results.filter(Boolean)
return { results: done, missing: args.diffs.map(d => d.id).filter(id => !done.some(r => r.id === id)) }
