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

// Hard-won: 8ba3d377e: the browser offered steering for a queued settings change.
test('the worker controls the action even when the displayed queue has settings first', () => {
  const command = { type: 'apply_queued_for', data: { active_prompt_id: 'turn-1', queued_command_id: 'model-change' } };
  const result = state({ active_prompt_id: 'turn-1', queued_prompts: [{ id: 'model-change' }, { id: 'prompt-1' }],
    turn_controls: { status: 'ready', primary: { kind: 'apply_queued', command }, interrupt: { type: 'cancel_turn_for', data: { active_prompt_id: 'turn-1' } }, message: 'Applies Fable after interrupting this turn.' } });
  assert.deepEqual(result.command, command);
  assert.equal(result.label, 'Apply queued');
  assert.match(result.message, /Fable/);
  assert.equal(result.interrupt.type, 'cancel_turn_for');
  assert.equal(state({ targeted_turn_control_supported: true, active_prompt_id: 'turn-1', queued_prompts: [{ id: 'prompt-1' }] }).command.type, 'cancel_turn_for');
});

test('reconnected pending and uncertain operations cannot silently become cancellation', () => {
  const session = { active_prompt_id: 'turn-1', queued_prompts: [{ id: 'queued-1' }],
    turn_controls: { status: 'pending', primary: null, interrupt: null } };
  assert.equal(state(session).pending, true);
  assert.equal(state(session).label, 'Steering…');
  session.turn_controls.status = 'unconfirmed';
  assert.equal(state(session).uncertain, true);
  assert.equal(state(session).label, 'Unconfirmed');
  session.turn_controls.status = 'pending';
  session.cancelling_prompt_id = 'turn-1';
  assert.equal(state(session).pending, true);
  assert.equal(state(session).label, 'Interrupting…');
});
