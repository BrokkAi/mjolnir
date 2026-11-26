import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { ViewerSession } from '../../../api/types';
import { emptyTranscript, useTranscriptStore } from '../../../stores/useTranscriptStore';
import { useSessionStore } from '../../../stores/useSessionStore';
import { jsonResponse, liveRow, mockFetch } from '../../../test-utils';
import { Composer } from '../Composer';

type Handler = (url: string, init?: RequestInit) => Response | Promise<Response>;

function installRoutes(extra: Record<string, Handler> = {}) {
  return mockFetch({
    '/api/v1/sessions/s1/client-state': () =>
      jsonResponse({ draft: '', through_event_ordinal: 0 }),
    '/api/v1/sessions/s1/draft': () => new Response(null, { status: 202 }),
    '/api/v1/actions': () => new Response(null, { status: 202 }),
    ...extra,
  });
}

function actionBodies(calls: { url: string; init?: RequestInit }[]) {
  return calls
    .filter(call => call.url === '/api/v1/actions')
    .map(call => JSON.parse(String(call.init?.body)) as Record<string, unknown>);
}

function session(overrides: Partial<ViewerSession> = {}): ViewerSession {
  return liveRow('s1', overrides);
}

beforeEach(() => {
  useSessionStore.getState().handleSignedOut();
  useTranscriptStore.getState().clearAll();
  useTranscriptStore.setState({
    entries: {
      s1: {
        ...emptyTranscript('s1'),
        hasLoaded: true,
        latestSeq: 4,
      },
    },
    order: ['s1'],
    activeId: 's1',
  });
});

afterEach(() => {
  cleanup();
  useSessionStore.getState().handleSignedOut();
  useTranscriptStore.getState().clearAll();
  vi.unstubAllGlobals();
});

describe('Composer', () => {
  it('submits a prompt with a command_id, an optimistic row, and re-attaches follow', async () => {
    const fetchMock = installRoutes();
    const follow = vi.fn();
    render(<Composer session={session()} onFollowBottom={follow} />);
    const textarea = screen.getByLabelText('Message');

    fireEvent.change(textarea, { target: { value: 'Fix the failing test' } });
    fireEvent.keyDown(textarea, { key: 'Enter' });

    await waitFor(() => expect(actionBodies(fetchMock.calls)).toHaveLength(1));
    const body = actionBodies(fetchMock.calls)[0];
    expect(body.action).toBe('prompt');
    expect(body.session_id).toBe('s1');
    expect(body.text).toBe('Fix the failing test');
    expect(typeof body.command_id).toBe('string');
    expect((body.command_id as string).length).toBeGreaterThan(0);
    expect(follow).toHaveBeenCalledTimes(1);
    await waitFor(() => expect((textarea as HTMLTextAreaElement).value).toBe(''));

    // The local row is keyed by the same command id the server will echo.
    const pending = useTranscriptStore.getState().entries.s1.pendingLocalPrompts;
    expect(pending).toHaveLength(1);
    expect(pending[0].commandId).toBe(body.command_id);
    expect(pending[0].text).toBe('Fix the failing test');
  });

  it('keeps words typed while a send is in flight', async () => {
    let release: () => void = () => undefined;
    const gate = new Promise<void>(resolve => {
      release = resolve;
    });
    const fetchMock = installRoutes({
      '/api/v1/actions': async () => {
        await gate;
        return new Response(null, { status: 202 });
      },
    });
    render(<Composer session={session()} />);
    const textarea = screen.getByLabelText('Message') as HTMLTextAreaElement;

    fireEvent.change(textarea, { target: { value: 'first' } });
    fireEvent.keyDown(textarea, { key: 'Enter' });
    await waitFor(() => expect(actionBodies(fetchMock.calls)).toHaveLength(1));
    fireEvent.change(textarea, { target: { value: 'second' } });
    release();

    await waitFor(() => expect(screen.getByRole('button', { name: 'Send' })).toBeEnabled());
    expect(textarea.value).toBe('second');
  });

  it('runs a !command through run-shell with its own command_id', async () => {
    const fetchMock = installRoutes();
    render(<Composer session={session()} />);
    const textarea = screen.getByLabelText('Message');

    fireEvent.change(textarea, { target: { value: '!echo hello' } });
    fireEvent.keyDown(textarea, { key: 'Enter' });

    await waitFor(() => expect(actionBodies(fetchMock.calls)).toHaveLength(1));
    const body = actionBodies(fetchMock.calls)[0];
    expect(body.action).toBe('run-shell');
    expect(body.session_id).toBe('s1');
    expect(body.command).toBe('echo hello');
    expect(typeof body.command_id).toBe('string');
    expect(body.text).toBeUndefined();
    await waitFor(() =>
      expect(useTranscriptStore.getState().entries.s1.pendingLocalPrompts[0]?.text).toBe('!echo hello'),
    );
  });

  it('toggles plan mode with set-plan-mode', async () => {
    const fetchMock = installRoutes();
    render(<Composer session={session({ plan_mode_active: false })} />);

    fireEvent.click(screen.getByRole('button', { name: 'Enable plan mode' }));

    await waitFor(() => expect(actionBodies(fetchMock.calls)).toHaveLength(1));
    expect(actionBodies(fetchMock.calls)[0]).toEqual({
      action: 'set-plan-mode',
      session_id: 's1',
      active: true,
    });
  });

  it('disables prompting, shell, plan mode and attachments when capabilities are absent', () => {
    const fetchMock = installRoutes();
    const base = liveRow('s1');
    render(
      <Composer
        session={session({
          capabilities: {
            ...base.capabilities,
            prompt: false,
            run_shell: false,
            set_plan_mode: false,
          },
        })}
      />,
    );

    expect(screen.getByLabelText('Message')).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Send' })).toBeDisabled();
    expect(screen.queryByRole('button', { name: 'Enable plan mode' })).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Attach file' })).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Attach image' })).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: 'Send' }));
    expect(actionBodies(fetchMock.calls)).toHaveLength(0);
  });

  it('refuses a !command when run_shell is unavailable', async () => {
    const fetchMock = installRoutes();
    const base = liveRow('s1');
    render(
      <Composer
        session={session({ capabilities: { ...base.capabilities, run_shell: false } })}
      />,
    );
    const textarea = screen.getByLabelText('Message');

    fireEvent.change(textarea, { target: { value: '!ls' } });
    fireEvent.keyDown(textarea, { key: 'Enter' });

    expect(await screen.findByRole('alert')).toHaveTextContent('Shell commands are not available');
    expect(actionBodies(fetchMock.calls)).toHaveLength(0);
  });

  it('offers the image picker only when the harness supports prompt images', () => {
    installRoutes();
    const { rerender } = render(<Composer session={session()} />);
    expect(screen.queryByRole('button', { name: 'Attach image' })).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Attach file' })).toBeInTheDocument();

    rerender(<Composer session={session({ prompt_images_supported: true })} />);
    expect(screen.getByRole('button', { name: 'Attach image' })).toBeInTheDocument();
  });
});
