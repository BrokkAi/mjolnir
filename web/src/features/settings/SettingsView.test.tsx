import { act, cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { JsonValue, SettingsDocument, SettingsEdit, SettingsPage } from '../../api/types';
import { useUiStore } from '../../stores/useUiStore';
import SettingsView from './SettingsView';

const api = vi.hoisted(() => ({ getSettings: vi.fn(), getSettingsPage: vi.fn(), editSettings: vi.fn(), saveSettings: vi.fn() }));
vi.mock('../../api/client', async importOriginal => ({ ...await importOriginal(), ...api }));
vi.mock('./GitIdentityCard', () => ({ GitIdentityCard: () => null }));

function page(path: string[] = [], value = 'old'): SettingsPage {
  return { path, breadcrumb: ['Settings', ...path], help: '', removable: false, groups: [{ rows: [
    { path: ['name'], label: 'Name', control: 'text', value, placeholder: '', number: false },
    { path: ['secret'], label: 'Secret', control: 'hidden' },
  ] }] };
}
const options = { primary: { '0': null } };
const original = { name: 'old', subagents: { primary: [{ model: 'm' }] }, review: { reviewers: [{ model: 'm' }] } };
function draft(name = 'old'): JsonValue {
  return { ...original, name, _subagent_options: options, _reviewer_options: { '0': null }, _review_action_reason: 'Checking reviewer models…' };
}
function doc(value = draft()): SettingsDocument { return { original, draft: value, page: page() }; }
function response(p: SettingsPage, sub: JsonValue = {}, review: JsonValue = {}) {
  return { page: p, subagent_options: sub, reviewer_options: review };
}

beforeEach(() => {
  vi.clearAllMocks();
  useUiStore.setState({ route: { name: 'settings', path: [] }, implicitRoute: false, layers: [] });
  api.getSettings.mockResolvedValue(doc());
  api.getSettingsPage.mockImplementation(async (_draft: JsonValue, path: string[]) => response(page(path)));
  api.editSettings.mockImplementation(async (d: JsonValue, path: string[], edit: SettingsEdit) => {
    const name = edit.kind === 'set' && edit.path[0] === 'name' ? edit.text ?? 'old' : (d as { name: string }).name;
    return { draft: { ...(d as object), name }, page: page(path, name) };
  });
  api.saveSettings.mockImplementation(async (_original: JsonValue, d: JsonValue, path: string[]) => ({ original: d, draft: d, page: page(path, (d as { name: string }).name) }));
});
afterEach(() => { cleanup(); vi.useRealTimers(); });

async function changeName(value: string) {
  const input = screen.getByRole('textbox', { name: 'Name' });
  fireEvent.change(input, { target: { value } });
  fireEvent.blur(input);
  await waitFor(() => expect(api.editSettings).toHaveBeenCalled());
}

describe('SettingsView live settings', () => {
  it('ignores synthetic keys and object order, warns on unload, and clears dirty on revert and discard', async () => {
    api.getSettings.mockResolvedValue({ ...doc(), original: { name: 'old' } });
    render(<SettingsView path={[]} />);
    await screen.findByRole('textbox', { name: 'Name' });
    expect(screen.queryByText('Unsaved changes')).not.toBeInTheDocument();
    await changeName('new');
    await screen.findByText('Unsaved changes');
    const unload = new Event('beforeunload', { cancelable: true });
    window.dispatchEvent(unload);
    expect(unload.defaultPrevented).toBe(true);
    await changeName('old');
    await waitFor(() => expect(screen.queryByText('Unsaved changes')).not.toBeInTheDocument());
    await changeName('new');
    await screen.findByText('Unsaved changes');
    fireEvent.click(screen.getByRole('button', { name: 'Discard' }));
    await waitFor(() => expect(screen.queryByText('Unsaved changes')).not.toBeInTheDocument());
    const cleanUnload = new Event('beforeunload', { cancelable: true });
    window.dispatchEvent(cleanUnload);
    expect(cleanUnload.defaultPrevented).toBe(false);
  });

  it.each(['subagents', 'review'])('polls %s discovery until choices settle', async section => {
    const path = section === 'review' ? ['review', 'reviewers', '0'] : ['subagents', 'primary', '0'];
    const pending = page(path);
    pending.groups = [{ rows: [{ path: [...path, 'model'], label: 'Model', control: 'read_only', text: 'Discovering' }] }];
    const ready = page(path);
    ready.groups = [{ rows: [{ path: [...path, 'model'], label: 'Model', control: 'choice', value: 'm', choices: ['m', 'n', 'o', 'p'].map(value => ({ value, label: value })) }] }];
    const pendingOpts = { '0': { efforts_pending: true } };
    const readyOpts = { '0': { efforts_pending: false } };
    api.getSettingsPage.mockResolvedValueOnce(response(pending, { primary: pendingOpts }, pendingOpts))
      .mockResolvedValue(response(ready, { primary: readyOpts }, readyOpts));
    render(<SettingsView path={path} />);
    await screen.findByText('Discovering');
    expect(await screen.findByRole('combobox', { name: 'Model' }, { timeout: 3000 })).toBeInTheDocument();
    expect(api.getSettingsPage).toHaveBeenCalledTimes(2);
    await act(async () => { await new Promise(resolve => setTimeout(resolve, 1100)); });
    expect(api.getSettingsPage).toHaveBeenCalledTimes(2);
  });

  it('edits hidden secrets with a password input and does not submit an empty value', async () => {
    render(<SettingsView path={[]} />);
    const input = await screen.findByLabelText('Secret');
    expect(input).toHaveAttribute('type', 'password');
    expect(input).toHaveAttribute('autocomplete', 'new-password');
    fireEvent.blur(input);
    expect(api.editSettings).not.toHaveBeenCalled();
    fireEvent.change(input, { target: { value: 'secret replacement' } });
    fireEvent.keyDown(input, { key: 'Enter' });
    fireEvent.blur(input);
    await waitFor(() => expect(api.editSettings).toHaveBeenCalledTimes(1));
    expect(api.editSettings.mock.calls[0][2]).toEqual({ kind: 'set', path: ['secret'], text: 'secret replacement' });
    await waitFor(() => expect(input).toHaveValue(''));
  });

  it('serializes an edit caused by blur before a save click', async () => {
    render(<SettingsView path={[]} />);
    const input = await screen.findByRole('textbox', { name: 'Name' });
    await changeName('changed');
    await screen.findByText('Unsaved changes');
    api.editSettings.mockClear();
    let finish!: (value: unknown) => void;
    api.editSettings.mockImplementationOnce(() => new Promise(resolve => { finish = resolve; }));
    fireEvent.change(input, { target: { value: 'latest' } });
    fireEvent.blur(input);
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(api.editSettings).toHaveBeenCalledTimes(1));
    expect(api.saveSettings).not.toHaveBeenCalled();
    await act(async () => finish({ draft: draft('latest'), page: page([], 'latest') }));
    await waitFor(() => expect(api.saveSettings).toHaveBeenCalledTimes(1));
    expect(api.saveSettings.mock.calls[0][1]).toEqual(draft('latest'));
  });

  it('shows a refusal beside its field and keeps the rejected text', async () => {
    api.editSettings.mockResolvedValueOnce({ draft: draft(), page: page(), error: 'Invalid name' });
    render(<SettingsView path={[]} />);
    await screen.findByRole('textbox', { name: 'Name' });
    await changeName('bad');
    const alert = await screen.findByRole('alert');
    expect(alert).toHaveTextContent('Invalid name');
    expect(within(alert.parentElement as HTMLElement).getByRole('textbox', { name: 'Name' })).toHaveValue('bad');
    await changeName('good');
    await waitFor(() => expect(screen.queryByText('Invalid name')).not.toBeInTheDocument());
  });

  it.each(['escape', 'restore'])('clears a rejected value on %s and permits saving other edits', async method => {
    render(<SettingsView path={[]} />);
    const input = await screen.findByRole('textbox', { name: 'Name' });
    await changeName('accepted');
    await screen.findByText('Unsaved changes');
    api.editSettings.mockResolvedValueOnce({ draft: draft('accepted'), page: page([], 'accepted'), error: 'Invalid name' });
    fireEvent.change(input, { target: { value: 'bad' } });
    fireEvent.blur(input);
    await screen.findByText('Invalid name');
    if (method === 'escape') {
      fireEvent.keyDown(input, { key: 'Escape' });
    } else {
      fireEvent.change(input, { target: { value: 'accepted' } });
      fireEvent.blur(input);
    }
    expect(input).toHaveValue('accepted');
    await waitFor(() => expect(screen.queryByText('Invalid name')).not.toBeInTheDocument());
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(api.saveSettings).toHaveBeenCalledTimes(1));
  });

  it('clears rejected fields for removed entries and shifted array siblings', async () => {
    const path = ['review', 'reviewers', '0'];
    const parent = ['review', 'reviewers'];
    const entryPage = page(path);
    entryPage.removable = true;
    entryPage.groups[0].rows.push(
      { path: [...path, 'model'], label: 'First model', control: 'text', value: 'first', placeholder: '', number: false },
      { path: [...parent, '1', 'model'], label: 'Second model', control: 'text', value: 'second', placeholder: '', number: false },
      { path: ['subagents', 'model'], label: 'Other model', control: 'text', value: 'other', placeholder: '', number: false },
    );
    const parentPage = page(parent, 'changed');
    parentPage.groups[0].rows.push(entryPage.groups[0].rows.at(-1)!);
    api.getSettings.mockResolvedValue({ ...doc(), page: entryPage });
    api.getSettingsPage.mockImplementation(async (_draft: JsonValue, target: string[]) => response(target.length === 3 ? entryPage : parentPage));
    api.editSettings.mockImplementation(async (d: JsonValue, _target: string[], edit: SettingsEdit) => {
      if (edit.kind === 'remove') return { draft: d, page: parentPage };
      if (edit.kind === 'set' && edit.path[0] === 'review') return { draft: d, page: entryPage, error: `Invalid ${edit.path[2]}` };
      if (edit.kind === 'set' && edit.path[0] === 'subagents') return { draft: d, page: entryPage, error: 'Invalid other' };
      return { draft: draft('changed'), page: { ...entryPage, groups: [{ rows: [
        { path: ['name'], label: 'Name', control: 'text', value: 'changed', placeholder: '', number: false },
        ...entryPage.groups[0].rows.slice(2),
      ] }] } };
    });
    useUiStore.setState({ route: { name: 'settings', path } });
    function Harness() {
      const route = useUiStore(state => state.route);
      return route.name === 'settings' ? <SettingsView path={route.path} /> : null;
    }
    render(<Harness />);
    await screen.findByRole('textbox', { name: 'Name' });
    await changeName('changed');
    await screen.findByText('Unsaved changes');
    for (const label of ['First model', 'Second model', 'Other model']) {
      const field = screen.getByRole('textbox', { name: label });
      fireEvent.change(field, { target: { value: 'bad' } });
      fireEvent.blur(field);
    }
    await screen.findByText('Invalid 0');
    await screen.findByText('Invalid 1');
    await screen.findByText('Invalid other');
    fireEvent.click(screen.getByRole('button', { name: 'Remove this entry' }));
    await waitFor(() => expect(useUiStore.getState().route).toEqual({ name: 'settings', path: parent }));
    fireEvent.click(await screen.findByRole('button', { name: 'Save' }));
    await screen.findByText('Fix the rejected settings before saving.');
    expect(api.saveSettings).not.toHaveBeenCalled();
    fireEvent.keyDown(screen.getByRole('textbox', { name: 'Other model' }), { key: 'Escape' });
    await waitFor(() => expect(screen.queryByText('Invalid other')).not.toBeInTheDocument());
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(api.saveSettings).toHaveBeenCalledTimes(1));
  });

  it('retries an unchanged value after a failed request without duplicating Enter and blur', async () => {
    let fail!: (error: Error) => void;
    api.editSettings.mockImplementationOnce(() => new Promise((_resolve, reject) => { fail = reject; }));
    render(<SettingsView path={[]} />);
    const input = await screen.findByRole('textbox', { name: 'Name' });
    fireEvent.change(input, { target: { value: 'retry me' } });
    fireEvent.keyDown(input, { key: 'Enter' });
    fireEvent.blur(input);
    await waitFor(() => expect(api.editSettings).toHaveBeenCalledTimes(1));
    await act(async () => fail(new Error('Connection failed')));
    await screen.findByText('Connection failed');
    expect(input).toHaveValue('retry me');
    fireEvent.keyDown(input, { key: 'Enter' });
    fireEvent.blur(input);
    await waitFor(() => expect(api.editSettings).toHaveBeenCalledTimes(2));
    expect(api.editSettings.mock.calls[1][2]).toEqual({ kind: 'set', path: ['name'], text: 'retry me' });
    await screen.findByText('Unsaved changes');
    expect(input).toHaveValue('retry me');
  });

  it('queues a return to the accepted value while an earlier edit is pending', async () => {
    let finishFirst!: (value: unknown) => void;
    let finishSecond!: (value: unknown) => void;
    api.editSettings.mockImplementationOnce(() => new Promise(resolve => { finishFirst = resolve; }))
      .mockImplementationOnce(() => new Promise(resolve => { finishSecond = resolve; }));
    render(<SettingsView path={[]} />);
    const input = await screen.findByRole('textbox', { name: 'Name' });
    fireEvent.change(input, { target: { value: 'B' } });
    fireEvent.blur(input);
    await waitFor(() => expect(api.editSettings).toHaveBeenCalledTimes(1));
    fireEvent.change(input, { target: { value: 'old' } });
    fireEvent.blur(input);
    expect(api.editSettings).toHaveBeenCalledTimes(1);
    await act(async () => finishFirst({ draft: draft('B'), page: page([], 'B') }));
    await waitFor(() => expect(api.editSettings).toHaveBeenCalledTimes(2));
    expect(api.editSettings.mock.calls[1][0]).toEqual(draft('B'));
    expect(api.editSettings.mock.calls[1][2]).toEqual({ kind: 'set', path: ['name'], text: 'old' });
    expect(input).toHaveValue('old');
    await act(async () => finishSecond({ draft: draft(), page: page() }));
    expect(input).toHaveValue('old');
    await waitFor(() => expect(screen.queryByText('Unsaved changes')).not.toBeInTheDocument());
  });

  it('shows the review action reason and dispatches it once enabled', async () => {
    const disabled = page(['review']);
    disabled.actions = [{ id: 'review.autoconfigure', label: 'Autoconfigure', disabled_reason: 'Checking reviewer models…' }];
    const enabled = { ...disabled, actions: [{ id: 'review.autoconfigure', label: 'Autoconfigure' }] };
    api.getSettingsPage.mockResolvedValueOnce(response(disabled, {}, { '0': null })).mockResolvedValue(response(enabled, {}, { '0': {} }));
    render(<SettingsView path={['review']} />);
    expect(await screen.findByText('Checking reviewer models…')).toBeVisible();
    expect(screen.getByRole('button', { name: 'Autoconfigure' })).toBeDisabled();
    await waitFor(() => expect(screen.getByRole('button', { name: 'Autoconfigure' })).toBeEnabled(), { timeout: 3000 });
    fireEvent.click(screen.getByRole('button', { name: 'Autoconfigure' }));
    await waitFor(() => expect(api.editSettings).toHaveBeenCalled());
    expect(api.editSettings.mock.calls[0][2]).toEqual({ kind: 'action', id: 'review.autoconfigure' });
  });
  it('stops discovery after 30 seconds even while responses remain pending', async () => {
    const path = ['subagents', 'primary', '0'];
    api.getSettingsPage.mockResolvedValue(response(page(path), { primary: { '0': null } }, {}));
    render(<SettingsView path={path} />);
    await waitFor(() => expect(api.getSettingsPage).toHaveBeenCalledTimes(1));
    vi.useFakeTimers();
    for (let i = 0; i < 35; i++) await act(async () => { await vi.advanceTimersByTimeAsync(1000); });
    expect(api.getSettingsPage.mock.calls.length).toBeLessThanOrEqual(30);
    const count = api.getSettingsPage.mock.calls.length;
    await act(async () => { await vi.advanceTimersByTimeAsync(5000); });
    expect(api.getSettingsPage).toHaveBeenCalledTimes(count);
  });

  it('discards an in-flight edit and restores even a refused input', async () => {
    render(<SettingsView path={[]} />);
    const input = await screen.findByRole('textbox', { name: 'Name' });
    await changeName('changed');
    await screen.findByText('Unsaved changes');
    api.editSettings.mockClear();
    let finish!: (value: unknown) => void;
    api.editSettings.mockImplementationOnce(() => new Promise(resolve => { finish = resolve; }));
    fireEvent.change(input, { target: { value: 'unwanted' } });
    fireEvent.blur(input);
    await waitFor(() => expect(api.editSettings).toHaveBeenCalled());
    fireEvent.click(screen.getByRole('button', { name: 'Discard' }));
    await act(async () => finish({ draft: draft('unwanted'), page: page([], 'unwanted') }));
    await waitFor(() => expect(screen.getByText('Changes discarded.')).toBeInTheDocument());
    expect(screen.getByRole('textbox', { name: 'Name' })).toHaveValue('old');
    expect(screen.queryByText('Unsaved changes')).not.toBeInTheDocument();
  });

  it('returns to the root after a missing subpage', async () => {
    const { ApiError } = await import('../../api/client');
    api.getSettingsPage.mockRejectedValueOnce(new ApiError('missing', 404));
    useUiStore.setState({ route: { name: 'settings', path: ['missing'] } });
    function Harness() {
      const route = useUiStore(state => state.route);
      return route.name === 'settings' ? <SettingsView path={route.path} /> : null;
    }
    render(<Harness />);
    await waitFor(() => expect(useUiStore.getState().route).toEqual({ name: 'settings', path: [] }));
    expect(screen.getByRole('textbox', { name: 'Name' })).toBeInTheDocument();
  });

});
