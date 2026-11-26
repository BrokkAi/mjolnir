import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { App } from '../App';
import { useSessionStore } from '../stores/useSessionStore';
import { useTranscriptStore } from '../stores/useTranscriptStore';
import { useUiStore } from '../stores/useUiStore';
import { FakeEventSource, jsonResponse, liveRow, mockFetch, snapshot, transcript } from '../test-utils';

beforeEach(() => {
  FakeEventSource.reset();
  vi.stubGlobal('EventSource', FakeEventSource);
  location.hash = '';
});

afterEach(() => {
  cleanup();
  useSessionStore.getState().handleSignedOut();
  useTranscriptStore.getState().clearAll();
  useUiStore.setState({ layers: [], toasts: [], drawerOpen: false });
  vi.unstubAllGlobals();
});

describe('App shell', () => {
  it('shows the login view when the snapshot answers 401', async () => {
    mockFetch({ '/api/v1/snapshot': () => new Response('', { status: 401 }) });
    render(<App />);
    expect(await screen.findByRole('heading', { name: 'Unlock Mjolnir' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Enter' })).toBeDisabled();
  });

  it('shows the bottom tab bar on a top-level screen and hides it inside a conversation', async () => {
    mockFetch({
      '/api/v1/snapshot': () => jsonResponse(snapshot([liveRow('s1', { title: 'Fix the build' })])),
      '/api/v1/conversations/s1': () => jsonResponse(transcript()),
    });
    render(<App />);

    expect(await screen.findByRole('navigation', { name: 'Primary' })).toBeInTheDocument();
    const open = await screen.findByRole('button', { name: 'Fix the build' });

    fireEvent.click(open);
    await waitFor(() => expect(screen.queryByRole('navigation', { name: 'Primary' })).not.toBeInTheDocument());
    expect(location.hash).toBe('#conversation/s1');
    expect(screen.getAllByRole('heading', { level: 1 })[0]).toHaveTextContent('Fix the build');
    await waitFor(() => expect(useTranscriptStore.getState().entries.s1?.hasLoaded).toBe(true));
  });

  it('tells the person an upgrade is in progress', async () => {
    mockFetch({ '/api/v1/snapshot': () => jsonResponse(snapshot([])) });
    render(<App />);
    await screen.findByRole('navigation', { name: 'Primary' });
    act(() => useSessionStore.setState({ upgrading: true }));
    expect(await screen.findByText(/Mjolnir is upgrading/)).toBeInTheDocument();
  });
});
