import { useCallback } from 'react';
import { cleanup, render, screen, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { BrowserTranscriptEntry } from '../../../api/types';
import { entryKey } from '../../../stores/useTranscriptStore';

/**
 * The memo boundary is the reason a streaming packet does not re-render every
 * earlier row. MarkdownContent is replaced with a call counter so a render of
 * an unchanged row is observable instead of inferred.
 */
const markdownCalls = vi.hoisted(() => ({ byText: new Map<string, number>() }));

vi.mock('../../../components/MarkdownContent', () => ({
  MarkdownContent: ({ text }: { text: string }) => {
    markdownCalls.byText.set(text, (markdownCalls.byText.get(text) ?? 0) + 1);
    return <div data-testid="markdown">{text}</div>;
  },
}));

import { entry } from '../../../test-utils';
import { sameTranscriptRowProps, TranscriptRow } from '../TranscriptRow';

function Harness({ rows, streamingKey }: { rows: BrowserTranscriptEntry[]; streamingKey?: string }) {
  const release = useCallback(() => {}, []);
  return (
    <div>
      {rows.map(row => {
        const key = entryKey(row);
        return (
          <TranscriptRow
            key={key}
            entry={row}
            sessionId="s1"
            isStreaming={key === streamingKey}
            onBeforeExpand={release}
          />
        );
      })}
    </div>
  );
}

afterEach(cleanup);

describe('TranscriptRow', () => {
  it('does not re-render an unchanged row when another row streams', () => {
    const first = entry(1, { lines: ['first message'] });
    const second = entry(2, { lines: ['second message'] });
    const { rerender } = render(<Harness rows={[first, second]} />);
    expect(screen.getByText('first message')).toBeInTheDocument();
    markdownCalls.byText.clear();

    // The second entry grows; its object identity changes, the first's does not.
    const grown = { ...second, updated_seq: 3, lines: ['second message grows'] };
    rerender(<Harness rows={[first, grown]} />);

    expect(markdownCalls.byText.get('first message') ?? 0).toBe(0);
    expect(screen.getByText('second message grows')).toBeInTheDocument();
  });

  it('throttles a streaming reply without dropping its final text', async () => {
    const first = entry(1, { lines: ['stable row'] });
    const streaming = entry(2, { lines: ['partial'] });
    const { rerender } = render(<Harness rows={[first, streaming]} streamingKey={entryKey(streaming)} />);
    markdownCalls.byText.clear();

    const grown = { ...streaming, updated_seq: 9, lines: ['partial reply complete'] };
    rerender(<Harness rows={[first, grown]} streamingKey={entryKey(grown)} />);

    await waitFor(() => expect(screen.getByText('partial reply complete')).toBeInTheDocument());
    expect(markdownCalls.byText.get('stable row') ?? 0).toBe(0);
  });

  it('treats an equal-sequence row as unchanged and a revised row as changed', () => {
    const base = {
      entry: entry(1, { stable_id: 'msg-1' }),
      sessionId: 's1',
      isStreaming: false,
      onBeforeExpand: () => {},
    };
    expect(sameTranscriptRowProps(base, { ...base, entry: { ...base.entry } })).toBe(true);
    expect(sameTranscriptRowProps(base, { ...base, entry: { ...base.entry, updated_seq: 2 } })).toBe(false);
    expect(sameTranscriptRowProps(base, { ...base, isStreaming: true })).toBe(false);
    expect(sameTranscriptRowProps(base, { ...base, sessionId: 's2' })).toBe(false);
  });
});
