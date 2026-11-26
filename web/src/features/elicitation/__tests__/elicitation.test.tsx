import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type {
  ElicitationField,
  ElicitationRequest,
  ViewerSession,
} from '../../../api/types';
import { ALONGSIDE_OPTION } from '../../../api/types';
import { useSessionStore } from '../../../stores/useSessionStore';
import { useUiStore } from '../../../stores/useUiStore';
import { liveRow, mockFetch } from '../../../test-utils';
import { ElicitationHost } from '../ElicitationHost';

const originalMatchMedia = window.matchMedia;

function setViewport(desktop: boolean) {
  window.matchMedia = ((query: string) => ({
    matches: query.includes('min-width: 640px') ? desktop : false,
    media: query,
    onchange: null,
    addEventListener: () => {},
    removeEventListener: () => {},
    addListener: () => {},
    removeListener: () => {},
    dispatchEvent: () => false,
  })) as unknown as typeof window.matchMedia;
}

function request(fields: ElicitationField[], overrides: Partial<ElicitationRequest> = {}): ElicitationRequest {
  return {
    id: 'e1',
    message: 'Choose how to continue.',
    fields,
    ...overrides,
  };
}

function sessionFor(elicitation: ElicitationRequest, overrides: Partial<ViewerSession> = {}): ViewerSession {
  return liveRow('s1', {
    pending_elicitations: [elicitation],
    ...overrides,
  });
}

function actionCalls(calls: { url: string; init?: RequestInit }[]) {
  return calls
    .filter(call => call.url === '/api/v1/actions')
    .map(call => JSON.parse(String(call.init?.body)) as Record<string, unknown>);
}

beforeEach(() => {
  sessionStorage.clear();
  setViewport(true);
  useSessionStore.getState().handleSignedOut();
  useUiStore.setState({ layers: [] });
});

afterEach(() => {
  cleanup();
  window.matchMedia = originalMatchMedia;
  vi.unstubAllGlobals();
});

describe('elicitation rendering', () => {
  it('renders the prompt and option descriptions as Markdown and a full preview with one scrollport', () => {
    const preview = Array.from({ length: 9 }, (_, index) => `preview line ${index + 1}`).join('\n');
    const elicitation = request(
      [
        {
          id: 'choice',
          title: 'Pick one',
          kind: 'single_select',
          required: false,
          options: [
            {
              value: 'a',
              title: 'Alpha (Recommended)',
              description: '**Descriptive** option',
              preview,
            },
          ],
        },
      ],
      {
        message: '**Bold prompt** with [a link](https://example.com).',
        description: '## Details heading',
      },
    );
    mockFetch({});
    render(<ElicitationHost session={sessionFor(elicitation)} />);

    expect(screen.getByText('Bold prompt')).toBeInTheDocument();
    expect(screen.getByRole('link', { name: 'a link' })).toHaveAttribute(
      'href',
      'https://example.com',
    );
    expect(screen.getByText('Descriptive')).toBeInTheDocument();
    expect(screen.getByRole('heading', { name: 'Details heading' })).toBeInTheDocument();
    expect(screen.getByText('Alpha')).toBeInTheDocument();
    expect(screen.getByText('Recommended')).toBeInTheDocument();
    expect(screen.getByText(/preview line 1/)).toBeInTheDocument();

    const expand = screen.getByRole('button', { name: 'Expand preview' });
    fireEvent.click(expand);
    expect(screen.getByRole('button', { name: 'Collapse preview' })).toBeInTheDocument();
    // Toggling the preview must not select the option it lives inside.
    expect(screen.getByRole('radio', { name: /Alpha/ })).not.toBeChecked();

    const bodies = screen.getAllByTestId('elicitation-sheet-body');
    expect(bodies).toHaveLength(1);
    expect(bodies[0].className).toContain('overflow-y-auto');
    expect(bodies[0].className).toContain('overscroll-contain');
    let ancestor = bodies[0].parentElement;
    while (ancestor && ancestor !== document.body) {
      expect(ancestor.className).not.toContain('overflow-y-auto');
      ancestor = ancestor.parentElement;
    }
  });

  it('opens the full-height sheet once on a phone and collapses to the dock without losing the draft', async () => {
    setViewport(false);
    const elicitation = request([
      { id: 'name', title: 'Name', kind: 'text', required: true },
    ]);
    mockFetch({});
    const view = render(<ElicitationHost session={sessionFor(elicitation)} />);

    expect(screen.getByTestId('elicitation-dock')).toBeInTheDocument();
    expect(await screen.findByRole('dialog')).toBeInTheDocument();
    fireEvent.change(screen.getByLabelText(/Name/), { target: { value: 'Grace' } });

    fireEvent.click(screen.getByRole('button', { name: 'Peek transcript' }));
    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
    expect(screen.getByTestId('elicitation-dock')).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: 'Answer' }));
    expect(await screen.findByRole('dialog')).toBeInTheDocument();
    expect(screen.getByLabelText(/Name/)).toHaveValue('Grace');
    expect(sessionStorage.getItem('mj-elicit:s1:e1')).toBeTruthy();

    view.unmount();
    render(<ElicitationHost session={sessionFor(elicitation)} />);
    expect(await screen.findByRole('dialog')).toBeInTheDocument();
    expect(screen.getByLabelText(/Name/)).toHaveValue('Grace');
    expect(document.body.className).toBe('');
  });
});

describe('elicitation companion roles', () => {
  function renderOne(fields: ElicitationField[]) {
    const fetchMock = mockFetch({
      '/api/v1/actions': () => new Response(null, { status: 202 }),
    });
    render(<ElicitationHost session={sessionFor(request(fields))} />);
    return fetchMock;
  }

  it('treats an absent custom_answer_option as an Alternative that replaces the pick', async () => {
    const fetchMock = renderOne([
      {
        id: 'q',
        title: 'Question',
        kind: 'single_select',
        required: false,
        options: [
          { value: 'a', title: 'Option A' },
          { value: 'b', title: 'Option B' },
        ],
      },
      {
        id: 'q_other',
        title: 'Other answer',
        kind: 'text',
        required: false,
        custom_answer_for: 'q',
      },
    ]);

    expect(screen.queryByRole('textbox', { name: 'Other answer' })).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('radio', { name: /Other answer/ }));
    const textarea = await screen.findByRole('textbox', { name: 'Other answer' });
    fireEvent.change(textarea, { target: { value: 'my own answer' } });
    fireEvent.click(screen.getByRole('button', { name: /Submit/ }));

    await waitFor(() => expect(actionCalls(fetchMock.calls)).toHaveLength(1));
    expect(actionCalls(fetchMock.calls)[0]).toEqual({
      action: 'respond-elicitation',
      session_id: 's1',
      elicitation_id: 'e1',
      response: { action: 'accept', content: { q_other: 'my own answer' } },
    });
  });

  it('renders an Alongside companion beside any pick and sends both values', async () => {
    const fetchMock = renderOne([
      {
        id: 'q',
        title: 'Question',
        kind: 'single_select',
        required: false,
        options: [
          { value: 'a', title: 'Option A' },
          { value: 'b', title: 'Option B' },
        ],
      },
      {
        id: 'q_note',
        title: 'Add a note',
        kind: 'text',
        required: false,
        custom_answer_for: 'q',
        custom_answer_option: ALONGSIDE_OPTION,
      },
    ]);

    const note = screen.getByRole('textbox', { name: 'Add a note' });
    fireEvent.change(note, { target: { value: 'extra detail' } });
    fireEvent.click(screen.getByRole('radio', { name: /Option A/ }));
    fireEvent.click(screen.getByRole('button', { name: /Submit/ }));

    await waitFor(() => expect(actionCalls(fetchMock.calls)).toHaveLength(1));
    expect(actionCalls(fetchMock.calls)[0]).toMatchObject({
      response: {
        action: 'accept',
        content: { q: 'a', q_note: 'extra detail' },
      },
    });
  });

  it('reveals a Detail companion only for its option and sends it with that option', async () => {
    const fetchMock = renderOne([
      {
        id: 'q',
        title: 'Question',
        kind: 'single_select',
        required: false,
        options: [
          { value: 'a', title: 'Option A' },
          { value: 'b', title: 'Option B' },
        ],
      },
      {
        id: 'q_detail',
        title: 'Why B?',
        kind: 'text',
        required: false,
        custom_answer_for: 'q',
        custom_answer_option: 'b',
      },
    ]);

    expect(screen.queryByRole('textbox', { name: 'Why B?' })).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('radio', { name: /Option B/ }));
    fireEvent.change(await screen.findByRole('textbox', { name: 'Why B?' }), {
      target: { value: 'because B' },
    });
    fireEvent.click(screen.getByRole('button', { name: /Submit/ }));

    await waitFor(() => expect(actionCalls(fetchMock.calls)).toHaveLength(1));
    expect(actionCalls(fetchMock.calls)[0]).toMatchObject({
      response: {
        action: 'accept',
        content: { q: 'b', q_detail: 'because B' },
      },
    });
  });
});

describe('elicitation validation and payloads', () => {
  it('validates required, numeric, and multi-select limits before dispatching', async () => {
    const fetchMock = mockFetch({
      '/api/v1/actions': () => new Response(null, { status: 202 }),
    });
    const elicitation = request([
      { id: 'name', title: 'Name', kind: 'text', required: true },
      { id: 'age', title: 'Age', kind: 'integer', required: true, minimum: 5 },
      {
        id: 'tags',
        title: 'Tags',
        kind: 'multi_select',
        required: false,
        min_items: 2,
        options: [
          { value: 'x', title: 'X tag' },
          { value: 'y', title: 'Y tag' },
        ],
      },
      { id: 'ratio', title: 'Ratio', kind: 'number', required: false, maximum: 10 },
    ]);
    render(<ElicitationHost session={sessionFor(elicitation)} />);

    fireEvent.click(screen.getByRole('button', { name: /Next/ }));
    expect(await screen.findByText('Name is required.')).toBeInTheDocument();
    expect(actionCalls(fetchMock.calls)).toHaveLength(0);

    fireEvent.change(screen.getByLabelText(/Name/), { target: { value: 'Ada' } });
    fireEvent.click(screen.getByRole('button', { name: /Next/ }));
    fireEvent.change(screen.getByLabelText(/Age/), { target: { value: '2' } });
    fireEvent.click(screen.getByRole('button', { name: /Next/ }));
    expect(await screen.findByText('Age must be at least 5.')).toBeInTheDocument();

    fireEvent.change(screen.getByLabelText(/Age/), { target: { value: '7' } });
    fireEvent.click(screen.getByRole('button', { name: /Next/ }));
    fireEvent.click(screen.getByRole('checkbox', { name: /X tag/ }));
    fireEvent.click(screen.getByRole('button', { name: /Next/ }));
    expect(await screen.findByText('Tags needs at least 2 selections.')).toBeInTheDocument();

    fireEvent.click(screen.getByRole('checkbox', { name: /Y tag/ }));
    fireEvent.click(screen.getByRole('button', { name: /Next/ }));
    fireEvent.change(screen.getByLabelText(/Ratio/), { target: { value: '11' } });
    fireEvent.click(screen.getByRole('button', { name: /Submit/ }));
    expect(await screen.findByText('Ratio must be at most 10.')).toBeInTheDocument();
    expect(actionCalls(fetchMock.calls)).toHaveLength(0);

    fireEvent.change(screen.getByLabelText(/Ratio/), { target: { value: '2.5' } });
    fireEvent.click(screen.getByRole('button', { name: /Submit/ }));
    await waitFor(() => expect(actionCalls(fetchMock.calls)).toHaveLength(1));
  });

  it('sends the exact accept payload, then clears its draft', async () => {
    const fetchMock = mockFetch({
      '/api/v1/actions': () => new Response(null, { status: 202 }),
    });
    const elicitation = request([
      { id: 'name', title: 'Name', kind: 'text', required: true },
      {
        id: 'choice',
        title: 'Choice',
        kind: 'single_select',
        required: true,
        default: 'a',
        options: [
          { value: 'a', title: 'A' },
          { value: 'b', title: 'B' },
        ],
      },
      {
        id: 'tags',
        title: 'Tags',
        kind: 'multi_select',
        required: false,
        options: [
          { value: 'x', title: 'X' },
          { value: 'y', title: 'Y' },
        ],
      },
      { id: 'flag', title: 'Flag', kind: 'boolean', required: false, default: false },
      { id: 'count', title: 'Count', kind: 'integer', required: false, minimum: 1, maximum: 10 },
      { id: 'ratio', title: 'Ratio', kind: 'number', required: false },
    ]);
    render(<ElicitationHost session={sessionFor(elicitation)} />);

    fireEvent.change(screen.getByLabelText(/Name/), { target: { value: 'Grace' } });
    fireEvent.click(screen.getByRole('button', { name: /Next/ }));
    fireEvent.click(screen.getByRole('button', { name: /Next/ }));
    fireEvent.click(screen.getByRole('checkbox', { name: /X/ }));
    fireEvent.click(screen.getByRole('checkbox', { name: /Y/ }));
    fireEvent.click(screen.getByRole('button', { name: /Next/ }));
    fireEvent.click(screen.getByRole('checkbox', { name: 'No' })); // Flag -> true
    fireEvent.click(screen.getByRole('button', { name: /Next/ }));
    fireEvent.change(screen.getByLabelText(/Count/), { target: { value: '7' } });
    fireEvent.click(screen.getByRole('button', { name: /Next/ }));
    fireEvent.change(screen.getByLabelText(/Ratio/), { target: { value: '2.5' } });
    fireEvent.click(screen.getByRole('button', { name: /Submit/ }));

    await waitFor(() => expect(actionCalls(fetchMock.calls)).toHaveLength(1));
    expect(actionCalls(fetchMock.calls)[0]).toEqual({
      action: 'respond-elicitation',
      session_id: 's1',
      elicitation_id: 'e1',
      response: {
        action: 'accept',
        content: {
          name: 'Grace',
          choice: 'a',
          tags: ['x', 'y'],
          flag: true,
          count: 7,
          ratio: 2.5,
        },
      },
    });
    expect(sessionStorage.getItem('mj-elicit:s1:e1')).toBeNull();
  });

  it('submits decline and cancel exactly', async () => {
    let fetchMock = mockFetch({
      '/api/v1/actions': () => new Response(null, { status: 202 }),
    });
    const first = render(
      <ElicitationHost session={sessionFor(request([{ id: 'name', title: 'Name', kind: 'text', required: false }]))} />,
    );
    fireEvent.click(screen.getByRole('button', { name: 'Decline' }));
    await waitFor(() => expect(actionCalls(fetchMock.calls)).toHaveLength(1));
    expect(actionCalls(fetchMock.calls)[0]).toMatchObject({
      action: 'respond-elicitation',
      response: { action: 'decline' },
    });
    first.unmount();
    vi.unstubAllGlobals();

    fetchMock = mockFetch({
      '/api/v1/actions': () => new Response(null, { status: 202 }),
    });
    render(
      <ElicitationHost session={sessionFor(request([{ id: 'name', title: 'Name', kind: 'text', required: false }]))} />,
    );
    fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    await waitFor(() => expect(actionCalls(fetchMock.calls)).toHaveLength(1));
    expect(actionCalls(fetchMock.calls)[0]).toMatchObject({
      action: 'respond-elicitation',
      response: { action: 'cancel' },
    });
  });
});
