import { useState } from 'react';
import { ArrowDown, Clock, LoaderCircle } from 'lucide-react';
import { MarkdownContent } from '../../components/MarkdownContent';
import type { HistoryEntry } from '../../api/types';
import {
  entryKey,
  visibleOlderItems,
  type CachedTranscript,
  type PendingLocalPrompt,
} from '../../stores/useTranscriptStore';
import { pendingLocalPrompts } from './conversationData';
import type { ConversationScroll } from './useConversationScroll';
import { ToolDetailBody, useToolDetail } from './ToolCard';
import { TranscriptRow } from './TranscriptRow';
import { formatClock } from './format';

/**
 * The single transcript scroller. Nothing here positions itself with inline
 * styles or `scrollIntoView`; the scroll engine owns every correction, so a
 * streamed packet, an expanding card, and a history prepend all take the same
 * deliberate path.
 */

export interface TranscriptFeedProps {
  sessionId: string;
  cached: CachedTranscript;
  scroll: ConversationScroll;
  /** Key of the assistant reply still being written, if any. */
  streamingKey: string | null;
  onLoadEarlier: () => void;
}

const HISTORY_ROLE_LABEL: Record<string, string> = {
  User: 'You',
  user: 'You',
  Agent: 'Assistant',
  agent: 'Assistant',
  Message: 'Message',
  message: 'Message',
  Thought: 'Thinking',
  thought: 'Thinking',
  Tool: 'Tool',
  tool: 'Tool',
  Plan: 'Plan',
  plan: 'Plan',
  PlanProposal: 'Plan proposal',
  plan_proposal: 'Plan proposal',
  System: 'System',
  system: 'System',
};

function historyRoleLabel(item: HistoryEntry): string {
  return item.label || HISTORY_ROLE_LABEL[item.role] || item.role;
}

interface HistoryRowProps {
  item: HistoryEntry;
  sessionId: string;
  /** Runs just before the full detail opens, so follow is released first. */
  onBeforeExpand: () => void;
}

function HistoryRow({ item, sessionId, onBeforeExpand }: HistoryRowProps) {
  const assistant =
    item.role === 'Agent' || item.role === 'agent' || item.role === 'Message' || item.role === 'message';
  const clock = formatClock(item.recorded_at_ms);
  const [detailOpen, setDetailOpen] = useState(false);
  // History entries carry no update sequence; their detail does not change.
  const { detail, loading, error, retry } = useToolDetail(
    sessionId,
    item.stable_id,
    '',
    detailOpen && Boolean(item.tool_detail_trimmed),
  );
  return (
    <article className="rounded-lg border border-line/60 bg-surface/50 px-3 py-2">
      <header className="mb-1 flex items-center gap-2 text-[10.5px] text-muted">
        <span className="font-medium">{historyRoleLabel(item)}</span>
        {item.tool_detail_trimmed && (
          <button
            type="button"
            aria-expanded={detailOpen}
            onClick={() => {
              if (!detailOpen) onBeforeExpand();
              setDetailOpen(open => !open);
            }}
            className="min-h-6 rounded bg-raised px-1.5 py-0.5 text-fg hover:bg-line/60"
          >
            {detailOpen ? 'Hide detail' : 'Full detail'}
          </button>
        )}
        {clock && <time className="ml-auto tabular-nums">{clock}</time>}
      </header>
      {assistant ? (
        <MarkdownContent text={item.text} compact />
      ) : (
        <p className="whitespace-pre-wrap break-words text-[13px] leading-relaxed text-fg">{item.text}</p>
      )}
      {detailOpen && item.tool_detail_trimmed && (
        <div className="mt-2 space-y-2 border-t border-line pt-2 text-[13px] [overflow-anchor:none]" data-testid="history-detail">
          {loading && (
            <p className="flex items-center gap-2 text-xs text-muted" role="status">
              <LoaderCircle className="size-3.5 animate-spin" aria-hidden />
              Loading full tool output…
            </p>
          )}
          {error && (
            <div className="rounded-md border border-danger/40 bg-danger/10 p-2">
              <p role="alert" className="text-[12px] text-danger">
                Could not load the full tool output: {error}
              </p>
              <button
                type="button"
                onClick={retry}
                className="mt-1 rounded border border-line bg-surface px-2 py-1 text-[12px] text-fg"
              >
                Try again
              </button>
            </div>
          )}
          {detail && <ToolDetailBody detail={detail} />}
        </div>
      )}
    </article>
  );
}

function PendingPromptRow({ prompt }: { prompt: PendingLocalPrompt }) {
  return (
    <div className="flex justify-end py-1" data-testid="pending-prompt">
      <article className="max-w-[85%] min-w-0 rounded-2xl rounded-br-sm border border-line border-r-2 border-r-accent/40 bg-raised/70 px-3 py-2 opacity-90">
        <header className="mb-0.5 flex items-center justify-end gap-1.5 text-[10.5px] text-muted">
          <Clock className="size-3" aria-hidden />
          <span className="font-medium">Sending…</span>
        </header>
        <p className="whitespace-pre-wrap break-words text-[13.5px] leading-relaxed text-fg">{prompt.text}</p>
      </article>
    </div>
  );
}

function EmptyFeed() {
  return (
    <div className="flex flex-col items-center gap-2 px-4 py-16 text-center text-sm text-muted">
      <p>No messages yet. The first message will appear here.</p>
    </div>
  );
}

export function TranscriptFeed({ sessionId, cached, scroll, streamingKey, onLoadEarlier }: TranscriptFeedProps) {
  const entries = cached.orderedIds
    .map(id => cached.entriesById[id])
    .filter((entry): entry is NonNullable<typeof entry> => Boolean(entry));
  const older = visibleOlderItems(cached);
  const pending = pendingLocalPrompts(cached);
  const earlierSlot = cached.hasMoreHistory || cached.loadingOlder;

  return (
    <div className="relative flex min-h-0 flex-1 flex-col">
      <div
        ref={scroll.scrollerRef}
        onScroll={scroll.onScroll}
        className="flex-1 min-h-0 min-w-px overflow-y-auto overscroll-contain [overflow-anchor:none]"
        data-testid="conversation-scroll"
      >
        <div ref={scroll.contentRef} className="mx-auto w-full max-w-3xl px-3 pb-6 pt-1 md:px-5">
          {earlierSlot && (
            <div
              className="flex h-12 min-h-12 shrink-0 items-center justify-center"
              data-testid="load-earlier-slot"
            >
              <button
                type="button"
                onClick={() => {
                  // Reading earlier messages is not following the end.
                  scroll.releaseFollow();
                  onLoadEarlier();
                }}
                disabled={cached.loadingOlder}
                className="inline-flex min-h-8 items-center gap-2 rounded-full border border-line bg-surface px-3 py-1 text-xs font-medium text-muted hover:text-fg disabled:opacity-60"
              >
                {cached.loadingOlder && <LoaderCircle className="size-3 animate-spin" aria-hidden />}
                {cached.loadingOlder ? 'Loading earlier messages…' : 'Load earlier messages'}
              </button>
            </div>
          )}
          {older.length > 0 && (
            <section aria-label="Earlier messages" className="mb-2 flex flex-col gap-2 border-b border-line/60 pb-2">
              {older.map(item => (
                <HistoryRow
                  key={item.stable_id}
                  item={item}
                  sessionId={sessionId}
                  onBeforeExpand={scroll.releaseFollow}
                />
              ))}
            </section>
          )}
          <div className="flex flex-col gap-2" data-testid="transcript-rows">
            {entries.map(entry => {
              const key = entryKey(entry);
              const isStreaming = key === streamingKey;
              return (
                <TranscriptRow
                  key={key}
                  entry={entry}
                  sessionId={sessionId}
                  isStreaming={isStreaming}
                  onBeforeExpand={scroll.releaseFollow}
                  streamingRef={isStreaming ? scroll.streamingRef : undefined}
                />
              );
            })}
            {pending.map(prompt => (
              <PendingPromptRow key={prompt.commandId} prompt={prompt} />
            ))}
          </div>
          {entries.length === 0 && pending.length === 0 && older.length === 0 && !cached.loading && <EmptyFeed />}
        </div>
      </div>
      {scroll.showJumpPill && (
        <div className="pointer-events-none absolute inset-x-0 bottom-3 z-10 flex justify-center px-3">
          <button
            type="button"
            onClick={scroll.follow}
            className="pointer-events-auto inline-flex min-h-9 items-center gap-2 rounded-full border border-line bg-raised/95 px-3 py-1.5 text-xs font-medium text-fg shadow-lg backdrop-blur"
            data-testid="jump-to-bottom"
          >
            {scroll.streaming ? (
              <LoaderCircle className="size-3.5 animate-spin text-accent" aria-hidden />
            ) : (
              <ArrowDown className="size-3.5 text-accent" aria-hidden />
            )}
            <span>Jump to bottom</span>
            {scroll.streaming ? (
              <span className="rounded-full bg-accent/15 px-1.5 py-0.5 text-[10px] text-accent">Streaming</span>
            ) : scroll.hasNew ? (
              <span className="rounded-full bg-accent px-1.5 py-0.5 text-[10px] font-semibold text-accent-fg">New</span>
            ) : scroll.paused ? (
              <span className="rounded-full bg-raised px-1.5 py-0.5 text-[10px] text-muted">Reading</span>
            ) : null}
          </button>
        </div>
      )}
    </div>
  );
}
