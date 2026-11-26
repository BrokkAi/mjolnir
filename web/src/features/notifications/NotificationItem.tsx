// One inbox card: why it needs attention, enough text to judge it, and the one
// action worth taking. Full messages load only when a person asks for them.

import { BellOff, ChevronDown, ChevronUp, Clock, MessageSquare } from 'lucide-react';
import { useEffect, useState } from 'react';
import { getNotificationMessage } from '../../api/client';
import type {
  NotificationMessageSelector,
  NotificationStackEntry,
  SessionRow,
} from '../../api/types';
import { MarkdownContent } from '../../components/MarkdownContent';
import { useUiStore } from '../../stores/useUiStore';
import {
  badgeLabel,
  badgeTone,
  dismissalTarget,
  entrySelector,
  formSelector,
  notificationsIgnored,
  openRouteFor,
  relativeTime,
  sessionTimestamp,
  type InboxItem,
} from './notificationsLogic';

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function LazyMessage({
  sessionId,
  selector,
}: {
  sessionId: string;
  selector: NotificationMessageSelector;
}) {
  const [text, setText] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [attempt, setAttempt] = useState(0);
  const selectorKey = JSON.stringify(selector);

  useEffect(() => {
    let cancelled = false;
    setText(null);
    setError(null);
    getNotificationMessage(sessionId, selector).then(
      answer => {
        if (!cancelled) setText(answer.text || '');
      },
      failure => {
        if (!cancelled) setError(messageOf(failure));
      },
    );
    return () => {
      cancelled = true;
    };
  }, [sessionId, selectorKey, attempt]); // eslint-disable-line react-hooks/exhaustive-deps

  if (error) {
    return (
      <div className="mt-2 rounded-lg border border-danger/40 bg-danger/10 p-2">
        <p role="alert" className="break-words text-[13px] text-danger">
          Could not read the message: {error}
        </p>
        <button
          type="button"
          onClick={() => setAttempt(value => value + 1)}
          className="mt-1 min-h-9 rounded border border-line bg-surface px-2 text-xs font-semibold text-fg"
        >
          Retry
        </button>
      </div>
    );
  }
  if (text === null) {
    return (
      <p role="status" className="mt-2 text-[13px] text-muted">
        Loading message…
      </p>
    );
  }
  return (
    <div className="mt-2 rounded-lg border border-line bg-bg p-3">
      <MarkdownContent text={text || 'Message not available.'} compact />
    </div>
  );
}

function StackEntryRow({
  row,
  entry,
  now,
  expanded,
  onToggle,
}: {
  row: SessionRow;
  entry: NotificationStackEntry;
  now: number;
  expanded: boolean;
  onToggle: () => void;
}) {
  const key = `entry:${entry.completed_ordinal}`;
  return (
    <div className="min-w-0 rounded-lg border border-line bg-raised p-2.5">
      <div className="flex min-w-0 flex-wrap items-center gap-2">
        <MessageSquare className="size-4 shrink-0 text-muted" aria-hidden />
        <span className="min-w-0 flex-1 break-words text-[13px] font-medium text-fg">
          {entry.kind === 'needs_input' ? 'Waiting for your input' : 'Important update'}
        </span>
        {entry.completed_at_ms != null && (
          <span className="inline-flex shrink-0 items-center gap-1 text-[12px] text-muted">
            <Clock className="size-3" aria-hidden />
            {relativeTime(entry.completed_at_ms, now)}
          </span>
        )}
        <button
          type="button"
          onClick={onToggle}
          aria-expanded={expanded}
          aria-label={expanded ? 'Hide full message' : 'Show full message'}
          className="inline-flex min-h-11 items-center gap-1 rounded px-1.5 text-xs font-semibold text-accent"
        >
          {expanded ? <ChevronUp className="size-3.5" aria-hidden /> : <ChevronDown className="size-3.5" aria-hidden />}
          {expanded ? 'Hide' : 'Read'}
        </button>
      </div>
      {expanded && (
        <div data-message-key={key}>
          <LazyMessage sessionId={row.id} selector={entrySelector(entry)} />
        </div>
      )}
    </div>
  );
}

export interface NotificationItemProps {
  item: InboxItem;
  workspaceName?: string;
  now: number;
  onDismiss: (row: SessionRow) => Promise<void>;
  onMute: (row: SessionRow) => Promise<void>;
}

export function NotificationItem({ item, workspaceName, now, onDismiss, onMute }: NotificationItemProps) {
  const navigate = useUiStore(state => state.navigate);
  const { row, badge, timestamp } = item;
  const card = row.notification_card;
  const target = dismissalTarget(row);
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(() => new Set());
  const [busy, setBusy] = useState<'dismiss' | 'mute' | null>(null);
  const [error, setError] = useState<string | null>(null);

  const toggle = (key: string) => {
    setExpanded(current => {
      const next = new Set(current);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  };

  const open = () => {
    const destination = openRouteFor(row);
    if (destination === 'resume' && row.workspace_id) {
      navigate({ name: 'resume', workspaceId: row.workspace_id, sessionId: row.id, from: 'notifications' });
    } else {
      navigate({ name: 'conversation', sessionId: row.id, from: 'notifications' });
    }
  };

  const dismiss = async () => {
    if (!target || busy) return;
    setBusy('dismiss');
    setError(null);
    try {
      await onDismiss(row);
    } catch (failure) {
      setError(messageOf(failure));
    } finally {
      setBusy(null);
    }
  };

  const mute = async () => {
    if (busy) return;
    setBusy('mute');
    setError(null);
    try {
      await onMute(row);
    } catch (failure) {
      setError(messageOf(failure));
    } finally {
      setBusy(null);
    }
  };

  const form = card?.form ?? null;
  const formKey = form ? `form:${form.request_id}` : '';
  const formSelectorValue = formSelector(row);
  const fallbackPreview = row.question_message || row.activity || '';

  return (
    <article
      data-testid={`notification-${row.id}`}
      className="min-w-0 rounded-xl border border-line bg-surface p-3 shadow-sm sm:p-4"
    >
      <div className="flex min-w-0 flex-wrap items-start gap-2">
        <button
          type="button"
          onClick={open}
          className="min-h-11 min-w-0 flex-1 basis-[14rem] rounded text-left text-[15px] font-semibold text-fg hover:text-accent"
        >
          <span className="break-words">{row.title || row.id}</span>
        </button>
        <span
          className={`shrink-0 rounded-full border px-2 py-0.5 text-[11px] font-semibold uppercase tracking-wide ${badgeTone(badge)}`}
        >
          {badgeLabel(badge)}
        </span>
      </div>
      <div className="mt-1 flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 text-[12px] text-muted">
        {workspaceName && (
          <span className="max-w-full break-words rounded border border-line bg-raised px-1.5 py-0.5">
            {workspaceName}
          </span>
        )}
        {row.project_label && (
          <span className="max-w-full break-words rounded border border-line bg-raised px-1.5 py-0.5">
            {row.project_label}
          </span>
        )}
        <span>{relativeTime(timestamp, now)}</span>
        {notificationsIgnored(row) && <span>Muted</span>}
      </div>

      <div className="mt-3 min-w-0 space-y-2">
        {card && card.entries.map(entry => (
          <StackEntryRow
            key={entry.completed_ordinal}
            row={row}
            entry={entry}
            now={now}
            expanded={expanded.has(`entry:${entry.completed_ordinal}`)}
            onToggle={() => toggle(`entry:${entry.completed_ordinal}`)}
          />
        ))}
        {form && (
          <div className="min-w-0 rounded-lg border border-line bg-raised p-2.5">
            <div className="flex min-w-0 flex-wrap items-center gap-2">
              <MessageSquare className="size-4 shrink-0 text-muted" aria-hidden />
              <span className="min-w-0 flex-1 break-words text-[13px] font-medium text-fg">
                {form.source === 'elicitation'
                  ? 'Question waiting'
                  : form.source === 'review_findings'
                    ? 'Review findings'
                    : 'Form waiting'}
              </span>
              <button
                type="button"
                onClick={() => toggle(formKey)}
                aria-expanded={expanded.has(formKey)}
                aria-label={expanded.has(formKey) ? 'Hide form message' : 'Show form message'}
                className="inline-flex min-h-11 items-center gap-1 rounded px-1.5 text-xs font-semibold text-accent"
              >
                {expanded.has(formKey) ? (
                  <ChevronUp className="size-3.5" aria-hidden />
                ) : (
                  <ChevronDown className="size-3.5" aria-hidden />
                )}
                {expanded.has(formKey) ? 'Hide' : 'Read'}
              </button>
            </div>
            {!expanded.has(formKey) && (
              <p className="mt-1 line-clamp-2 min-w-0 break-words text-[13px] text-fg">
                {form.message}
              </p>
            )}
            {expanded.has(formKey) &&
              (formSelectorValue ? (
                <LazyMessage sessionId={row.id} selector={formSelectorValue} />
              ) : (
                <div className="mt-2 rounded-lg border border-line bg-bg p-3">
                  <MarkdownContent text={form.message || 'Message not available.'} compact />
                </div>
              ))}
          </div>
        )}
        {!card && fallbackPreview && (
          <p className="line-clamp-3 min-w-0 break-words whitespace-pre-wrap text-[13px] leading-relaxed text-fg">
            {fallbackPreview}
          </p>
        )}
        {!card && !fallbackPreview && (
          <p className="text-[13px] text-muted">
            {row.has_error || row.launch_error
              ? 'This session failed and may need a retry.'
              : 'This session needs a person.'}
          </p>
        )}
      </div>

      {error && (
        <p role="alert" className="mt-2 break-words text-[13px] font-medium text-danger">
          {error}
        </p>
      )}

      <div className="mt-3 flex flex-wrap items-center gap-2">
        <button
          type="button"
          onClick={open}
          className="min-h-11 rounded-lg bg-accent px-4 text-sm font-semibold text-accent-fg"
        >
          {openRouteFor(row) === 'resume' ? 'Resume session' : 'Open conversation'}
        </button>
        <button
          type="button"
          onClick={() => void dismiss()}
          disabled={!target || busy !== null}
          className="min-h-11 rounded-lg border border-line bg-raised px-3 text-sm font-semibold text-fg disabled:opacity-40"
        >
          {busy === 'dismiss' ? 'Dismissing…' : 'Dismiss'}
        </button>
        <button
          type="button"
          onClick={() => void mute()}
          disabled={busy !== null}
          aria-pressed={notificationsIgnored(row)}
          className="inline-flex min-h-11 items-center gap-1.5 rounded-lg border border-line bg-raised px-3 text-sm font-semibold text-fg disabled:opacity-40"
        >
          <BellOff className="size-4" aria-hidden />
          {busy === 'mute'
            ? 'Saving…'
            : notificationsIgnored(row)
              ? 'Unmute'
              : 'Mute'}
        </button>
        <span className="text-[12px] text-muted">
          Updated {relativeTime(sessionTimestamp(row), now)}
        </span>
      </div>
    </article>
  );
}
