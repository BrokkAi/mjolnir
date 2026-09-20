import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../../../mj-controller/src/web/viewer.js', import.meta.url), 'utf8');
const begin = source.indexOf('function turnControlState(session) {');
const end = source.indexOf('\nasync function submitTurnControl', begin);
const context = vm.createContext({});
vm.runInContext(source.slice(begin, end), context);
const state = session => JSON.parse(JSON.stringify(context.turnControlState(session)));

test('queued input produces an identity-bound steer instead of cancellation', () => {
  const result = state({ active_prompt_id: 'turn-1', queued_prompts: [{ id: 'queued-1' }] });
  assert.deepEqual(result.command, { type: 'steer', data: { active_prompt_id: 'turn-1', queued_prompt_id: 'queued-1' } });
  assert.equal(result.label, 'Steer queued prompt');
  assert.equal(state({ active_prompt_id: 'turn-1' }).command.type, 'cancel_turn_for');
});

test('reconnected pending and uncertain operations cannot silently become cancellation', () => {
  const session = { active_prompt_id: 'turn-1', queued_prompts: [{ id: 'queued-1' }], steering: { status: 'pending' } };
  assert.equal(state(session).pending, true);
  assert.equal(state(session).label, 'Steering…');
  session.steering.status = 'unconfirmed';
  assert.equal(state(session).uncertain, true);
  assert.equal(state(session).label, 'Delivery unconfirmed');
  session.cancelling_prompt_id = 'turn-1';
  assert.equal(state(session).pending, true);
  assert.equal(state(session).label, 'Stopping turn…');
});

test('failed steering offers a decision rather than reporting applied input', () => {
  assert.equal(state({ steering: { status: 'failed' } }).failed, true);
  assert.equal(state({ steering: { status: 'applied' } }).pending, false);
  assert.equal(state({}).command, null);
});
