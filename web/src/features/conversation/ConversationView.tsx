import { useCallback, useMemo, useState, type ReactNode } from 'react';
import { AlertTriangle, LoaderCircle, RefreshCw } from 'lucide-react';
import { isSummaryRow } from '../../api/types';
import type { ViewerSession } from '../../api/types';
import { ElicitationHost } from '../elicitation/ElicitationHost';
import { useSessionStore } from '../../stores/useSessionStore';
import { isHistoryOnly, useTranscriptStore, visibleOlderItems } from '../../stores/useTranscriptStore';
import { Composer } from './Composer';
import { ConversationHeader } from './ConversationHeader';
import { LiveActivityBars } from './LiveActivityBars';
import { SessionDiffSheet } from './SessionDiffSheet';
import { SessionInfoSheet } from './SessionInfoSheet';
import { SubagentsSheet } from './SubagentsSheet';
import { TranscriptFeed } from './TranscriptFeed';
import {
  selectStreamingEntryKey,
  sessionCanMarkRead,
  transcriptContentVersion,
} from './conversationData';
import { useConversationScroll } from './useConversationScroll';
import { describeError } from './format';

/**
 * Props the shell passes. Keep this shape when replacing the placeholder.
 */
export type ConversationViewProps = {
  sessionId: string;
  subagentParentId?: string;
  childKind?: 'reviewer';
  from?: 'notifications';
};

type Sheet = 'subagents' | 'diff' | 'info';

function Centered({ children }: { children: ReactNode }) {
  return (
    <div className="flex min-h-0 flex-1 flex-col items-center justify-center gap-3 px-6 py-10 text-center text-sm text-muted">
      {children}
    </div>
  );
}

function ErrorCard({ message, onRetry }: { message: string; onRetry: () => void }) {
  return (
    <div className="mx-auto my-6 w-full max-w-md rounded-xl border border-danger/40 bg-danger/10 p-3">
      <p role="alert" className="flex items-start gap-2 text-[13px] text-danger">
        <AlertTriangle className="mt-0.5 size-4 shrink-0" aria-hidden />
        <span className="min-w-0 flex-1 break-words">{message}</span>
      </p>
      <button
        type="button"
        onClick={onRetry}
        className="mt-2 inline-flex min-h-8 items-center gap-1.5 rounded-lg border border-line bg-surface px-2.5 py-1 text-xs text-fg"
      >
        <RefreshCw className="size-3.5" aria-hidden />
        Try again
      </button>
    </div>
  );
}

function TranscriptErrorBanner({ message, onRetry }: { message: string; onRetry: () => void }) {
  return (
    <div className="border-b border-danger/40 bg-danger/10 px-3 py-1.5">
      <div className="mx-auto flex w-full max-w-3xl items-center gap-2 text-[12px] text-danger">
        <p role="alert" className="flex min-w-0 flex-1 items-start gap-2">
          <AlertTriangle className="mt-0.5 size-3.5 shrink-0" aria-hidden />
          <span className="min-w-0 flex-1 break-words">{message}</span>
        </p>
        <button
          type="button"
          onClick={onRetry}
          className="inline-flex min-h-8 shrink-0 items-center gap-1.5 rounded-lg border border-line bg-surface px-2.5 py-1 text-xs text-fg"
        >
          <RefreshCw className="size-3.5" aria-hidden />
          Retry
        </button>
      </div>
    </div>
  );
}

function BlockedNotice({ kind }: { kind: 'transitioning' | 'unavailable' }) {
  const text =
    kind === 'transitioning'
      ? 'This conversation is moving between states. The transcript resumes automatically.'
      : 'The daemon cannot serve this conversation right now. It will recover when the session is live.';
  return (
    <div className="border-b border-line bg-raised/60 px-3 py-1.5" role="status">
      <p className="mx-auto flex w-full max-w-3xl items-center gap-2 text-[11.5px] text-muted">
        {kind === 'transitioning' && <LoaderCircle className="size-3.5 animate-spin" aria-hidden />}
        <span className="min-w-0 flex-1">{text}</span>
      </p>
    </div>
  );
}

function notLiveMessage(session: ViewerSession): string {
  switch (session.lifecycle) {
    case 'suspended':
      return 'This session is suspended. Resume it from its workspace to continue the conversation.';
    case 'starting':
      return 'This session is starting. The conversation appears once it is live.';
    case 'suspending':
      return 'This session is suspending. Its conversation is paused.';
    case 'failed':
      return session.launch_error || 'This session failed to launch.';
    default:
      return 'This session is not accepting messages right now.';
  }
}

/**
 * The conversation screen.
 *
 * The shell's route hook owns opening and closing the conversation and fetching
 * detail for a summary row; this view only reads `useTranscriptStore` and
 * `useSessionStore`. It owns the single transcript scroller and the non-scrolling
 * bottom dock, so there is exactly one scrollport and one place where the
 * keyboard inset is applied.
 */
export default function ConversationView({ sessionId, subagentParentId, childKind }: ConversationViewProps) {
  const session = useSessionStore(state => state.sessionsById[sessionId]);
  const cached = useTranscriptStore(state => state.entries[sessionId]);
  const detailLoading = useSessionStore(state => Boolean(state.detailLoadingIds[sessionId]));
  const detailError = useSessionStore(state => state.detailErrors[sessionId]);
  const [sheet, setSheet] = useState<Sheet | null>(null);
  const closeSheet = useCallback(() => setSheet(null), []);

  const detail = session && !isSummaryRow(session) ? session : undefined;

  const streamingKey = useMemo(() => selectStreamingEntryKey(session, cached), [session, cached]);
  const scroll = useConversationScroll({
    sessionId,
    hasLoaded: Boolean(cached?.hasLoaded),
    contentVersion: cached ? transcriptContentVersion(cached) : 'empty',
    olderCount: cached ? visibleOlderItems(cached).length : 0,
    latestSeq: cached?.latestSeq ?? 0,
    streaming: streamingKey,
    canMarkRead: sessionCanMarkRead(session),
  });

  const retryTranscript = useCallback(() => {
    void useTranscriptStore.getState().syncConversation(sessionId, { force: true });
    if (session && isSummaryRow(session)) {
      void useSessionStore
        .getState()
        .ensureSessionDetail(sessionId)
        .catch(error => console.warn('Could not load session detail', describeError(error)));
    }
  }, [session, sessionId]);

  let body: ReactNode;
  if (!session) {
    body = (
      <Centered>
        <LoaderCircle className="size-5 animate-spin" aria-hidden />
        <p>Loading session…</p>
      </Centered>
    );
  } else if (isSummaryRow(session)) {
    body = detailError ? (
      <ErrorCard message={detailError} onRetry={retryTranscript} />
    ) : (
      <Centered>
        <LoaderCircle className="size-5 animate-spin" aria-hidden />
        <p>Loading session details…</p>
        {(session.pending_elicitation_count > 0 || session.queued_prompt_count > 0) && (
          <p className="text-xs">
            {session.pending_elicitation_count > 0 && `${session.pending_elicitation_count} pending question(s)`}
            {session.pending_elicitation_count > 0 && session.queued_prompt_count > 0 && ' · '}
            {session.queued_prompt_count > 0 && `${session.queued_prompt_count} queued prompt(s)`}
          </p>
        )}
        {detailLoading && <span className="sr-only">Fetching session detail</span>}
      </Centered>
    );
  } else if (!cached) {
    body = (
      <Centered>
        <LoaderCircle className="size-5 animate-spin" aria-hidden />
        <p>Loading conversation…</p>
      </Centered>
    );
  } else {
    const blocked = cached.blocked;
    body = (
      <div className="flex min-h-0 flex-1 flex-col">
        {blocked && <BlockedNotice kind={blocked} />}
        {cached.hasLoaded && cached.error && (
          <TranscriptErrorBanner message={cached.error} onRetry={retryTranscript} />
        )}
        {cached.hasLoaded ? (
          <TranscriptFeed
            sessionId={sessionId}
            cached={cached}
            scroll={scroll}
            streamingKey={streamingKey}
            onLoadEarlier={() => void useTranscriptStore.getState().loadEarlierHistory(sessionId)}
          />
        ) : cached.error ? (
          <ErrorCard message={cached.error} onRetry={retryTranscript} />
        ) : (
          <Centered>
            <LoaderCircle className="size-5 animate-spin" aria-hidden />
            <p>{cached.loading ? 'Loading conversation…' : 'Waiting for the transcript…'}</p>
          </Centered>
        )}
      </div>
    );
  }

  return (
    <div className="flex h-full min-h-0 flex-col overflow-hidden" data-testid="conversation-view">
      {session && (
        <ConversationHeader
          session={session}
          sessionId={sessionId}
          subagentParentId={subagentParentId}
          childKind={childKind}
          onOpenSubagents={() => setSheet('subagents')}
          onOpenDiff={() => setSheet('diff')}
          onOpenInfo={() => setSheet('info')}
        />
      )}

      {body}

      {detail && detail.capabilities.open ? (
        <>
          <LiveActivityBars session={detail} />
          <ElicitationHost session={detail} />
          <Composer session={detail} onFollowBottom={scroll.follow} />
        </>
      ) : (
        <div className="shrink-0 border-t border-line bg-surface px-3 pt-3 pb-keyboard pb-safe">
          <p className="mx-auto w-full max-w-3xl text-center text-[11.5px] text-muted">
            {session && isHistoryOnly(session)
              ? 'This session has stopped. Its stored history is shown read-only.'
              : detail
                ? notLiveMessage(detail)
                : 'This session is not live.'}
          </p>
        </div>
      )}

      {sheet === 'subagents' && session && (
        <SubagentsSheet
          sessionId={sessionId}
          subagentSessionIds={session.subagent_session_ids}
          reviewerSessionIds={session.reviewer_session_ids}
          nativeAgents={detail?.native_subagents}
          onClose={closeSheet}
        />
      )}
      {sheet === 'diff' && <SessionDiffSheet sessionId={sessionId} onClose={closeSheet} />}
      {sheet === 'info' && session && <SessionInfoSheet session={session} onClose={closeSheet} />}
    </div>
  );
}
