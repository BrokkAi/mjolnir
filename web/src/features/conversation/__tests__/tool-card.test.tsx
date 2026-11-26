import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { ToolCallDetailResponse } from '../../../api/types';
import { entry, jsonResponse, mockFetch } from '../../../test-utils';
import { resetToolDetailCache, ToolCard } from '../ToolCard';

const DETAIL_PATH = '/api/v1/sessions/s1/items/tool-1/detail';

function detailPayload(): ToolCallDetailResponse {
  return {
    status: 'full',
    stable_id: 'tool-1',
    entry: {
      start_seq: 1,
      seq: 2,
      role: 'Tool',
      text: '',
      recorded_at_ms: 1000,
      revision: 1,
      message_id: null,
      tool_call_id: 'call-1',
      tool_status: 'Completed',
      tool_content: ['tool output line'],
      tool_diffstats: [],
      tool_locations: [],
      plan: [],
    },
    body: { kind: 'system', text: '' },
    call: {},
    diffs: [
      {
        text: '@@ -1 +1 @@\n-old line\n+new line',
        insertions: 1,
        deletions: 1,
      },
    ],
    terminal_outputs: [{ terminal_id: 't1', output: 'hello from terminal', exit_code: 0 }],
  } as unknown as ToolCallDetailResponse;
}

function toolEntry(overrides: Parameters<typeof entry>[1] = {}) {
  return entry(1, {
    role: 'tool',
    label: 'Run tests',
    stable_id: 'tool-1',
    tool_detail_trimmed: true,
    tool_status: 'running',
    recorded_at_ms: 1000,
    tool_ended_at_ms: 2500,
    lines: ['summary line'],
    ...overrides,
  });
}

beforeEach(() => {
  resetToolDetailCache();
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe('ToolCard', () => {
  it('loads trimmed detail only after expanding, then serves it from the cache', async () => {
    const fetchMock = mockFetch({ [DETAIL_PATH]: () => jsonResponse(detailPayload()) });
    const onBeforeExpand = vi.fn();
    render(<ToolCard entry={toolEntry()} sessionId="s1" onBeforeExpand={onBeforeExpand} />);

    expect(fetchMock.calls).toHaveLength(0);
    expect(screen.queryByTestId('tool-card-body')).not.toBeInTheDocument();
    expect(screen.getByText('2s')).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: /Run tests, expand/ }));

    expect(onBeforeExpand).toHaveBeenCalledTimes(1);
    await waitFor(() => expect(screen.getByText('hello from terminal')).toBeInTheDocument());
    expect(fetchMock.calls).toHaveLength(1);
    expect(screen.getByText('+new line')).toBeInTheDocument();
    expect(screen.getByText('-old line')).toBeInTheDocument();

    // Collapsing and reopening must not refetch the detail.
    fireEvent.click(screen.getByRole('button', { name: /Run tests, collapse/ }));
    expect(screen.queryByTestId('tool-card-body')).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: /Run tests, expand/ }));
    await waitFor(() => expect(screen.getByText('hello from terminal')).toBeInTheDocument());
    expect(fetchMock.calls).toHaveLength(1);
    expect(onBeforeExpand).toHaveBeenCalledTimes(2);
  });

  it('shows structured disclosures and diffstat pills without fetching for an untrimmed row', () => {
    const fetchMock = mockFetch({});
    render(
      <ToolCard
        entry={toolEntry({
          tool_detail_trimmed: false,
          stable_id: undefined,
          tool_status: 'done',
          disclosures: [{ summary: ['12 files changed'], body: ['+ src/a.ts', '- src/b.ts'] }],
          diffstats: [{ path: 'src/a.ts', insertions: 3, deletions: 1 }],
        })}
        sessionId="s1"
        onBeforeExpand={() => {}}
      />,
    );

    fireEvent.click(screen.getByRole('button', { name: /Run tests, expand/ }));

    expect(screen.getByText('12 files changed')).toBeInTheDocument();
    expect(screen.getByText(/\+ src\/a\.ts/)).toBeInTheDocument();
    expect(screen.getByText('+3')).toBeInTheDocument();
    expect(screen.getByText('-1')).toBeInTheDocument();
    expect(fetchMock.calls).toHaveLength(0);
  });

  it('reports a failed detail fetch and retries on demand', async () => {
    let attempt = 0;
    const fetchMock = mockFetch({
      [DETAIL_PATH]: () => {
        attempt += 1;
        if (attempt === 1) return new Response('boom', { status: 500 });
        return jsonResponse(detailPayload());
      },
    });
    render(<ToolCard entry={toolEntry()} sessionId="s1" onBeforeExpand={() => {}} />);

    fireEvent.click(screen.getByRole('button', { name: /Run tests, expand/ }));
    expect(await screen.findByRole('alert')).toHaveTextContent('boom');

    fireEvent.click(screen.getByRole('button', { name: 'Try again' }));
    await waitFor(() => expect(screen.getByText('hello from terminal')).toBeInTheDocument());
    expect(fetchMock.calls).toHaveLength(2);
  });
});
