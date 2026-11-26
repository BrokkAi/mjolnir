// The Notifications Inbox: every session that needs the person, highest
// priority first, with AFK and this device's push controls in the header.

import { CheckCircle2 } from 'lucide-react';
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import type { SessionRow } from '../../api/types';
import { PushBanner } from '../push/PushBanner';
import { useSessionStore } from '../../stores/useSessionStore';
import { useUiStore } from '../../stores/useUiStore';
import { NotificationItem } from './NotificationItem';
import {
  dismissalTarget,
  inboxItems,
  notificationsIgnored,
  publishedDismissal,
  type AcceptedDismissal,
} from './notificationsLogic';

/** Props the shell passes. Keep this shape when replacing the placeholder. */
export type NotificationsViewProps = Record<string, never>;

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** Report trusted presence, throttled, only while auto-AFK is active. */
function useOperatorActivity(enabled: boolean): void {
  useEffect(() => {
    if (!enabled) return;
    let lastSent = 0;
    let inFlight = false;
    let dirty = false;
    let failed = false;
    let timer: number | undefined;

    function schedule() {
      if (!dirty || inFlight || failed || timer !== undefined) return;
      const wait = Math.max(0, 1000 - (Date.now() - lastSent));
      timer = window.setTimeout(() => {
        timer = undefined;
        send();
      }, wait);
    }

    function send() {
      if (!dirty || inFlight || failed) return;
      if (navigator.onLine === false) return;
      dirty = false;
      inFlight = true;
      lastSent = Date.now();
      useSessionStore
        .getState()
        .dispatchAction({ action: 'record-operator-activity' })
        .catch(() => {
          // A failed background signal must not surface once per keystroke.
          failed = true;
        })
        .finally(() => {
          inFlight = false;
          schedule();
        });
    }

    const observe = (event: Event) => {
      if (!event.isTrusted || failed) return;
      dirty = true;
      schedule();
    };

    const types = ['pointerdown', 'keydown', 'paste', 'input'] as const;
    for (const type of types) {
      document.addEventListener(type, observe, { capture: true, passive: true });
    }
    return () => {
      for (const type of types) {
        document.removeEventListener(type, observe, { capture: true });
      }
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [enabled]);
}

export default function NotificationsView(_props: NotificationsViewProps) {
  const meta = useSessionStore(state => state.meta);
  const sessionsById = useSessionStore(state => state.sessionsById);
  const dispatchAction = useSessionStore(state => state.dispatchAction);
  const navigate = useUiStore(state => state.navigate);

  const [accepted, setAccepted] = useState<Record<string, AcceptedDismissal>>({});
  const [muted, setMuted] = useState<Record<string, boolean>>({});
  const [afkPending, setAfkPending] = useState<boolean | null>(null);
  const [afkError, setAfkError] = useState<string | null>(null);
  const mounted = useRef(true);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  useOperatorActivity(
    Boolean(meta && meta.auto_afk_minutes > 0 && meta.operator_away !== true),
  );

  const rows = useMemo<SessionRow[]>(
    () =>
      Object.values(sessionsById).map(row => {
        const override = muted[row.id];
        if (override === undefined || notificationsIgnored(row) === override) return row;
        return { ...row, notifications_ignored: override } as SessionRow;
      }),
    [muted, sessionsById],
  );

  const items = useMemo(() => inboxItems(rows, accepted), [accepted, rows]);
  const now = meta?.server_time_ms ?? Date.now();
  const workspaceNames = useMemo(() => {
    const names: Record<string, string> = {};
    for (const workspace of meta?.workspaces ?? []) names[workspace.id] = workspace.name;
    return names;
  }, [meta?.workspaces]);

  const away = afkPending ?? meta?.operator_away === true;

  const toggleAway = async () => {
    if (afkPending !== null) return;
    const next = !away;
    setAfkPending(next);
    setAfkError(null);
    try {
      await dispatchAction({ action: 'set-operator-away', away: next });
    } catch (error) {
      if (mounted.current) setAfkError(messageOf(error));
    } finally {
      if (mounted.current) setAfkPending(null);
    }
  };

  const onDismiss = useCallback(
    async (row: SessionRow) => {
      const target = dismissalTarget(row);
      if (!target) throw new Error('This notification cannot be dismissed.');
      await dispatchAction({
        action: 'dismiss-notification',
        session_id: row.id,
        target,
      });
      setAccepted(current => ({
        ...current,
        [row.id]: {
          target: JSON.stringify(publishedDismissal(row)),
          card: Boolean(row.notification_card),
        },
      }));
    },
    [dispatchAction],
  );

  const onMute = useCallback(
    async (row: SessionRow) => {
      const ignored = !notificationsIgnored(row);
      await dispatchAction({
        action: 'set-session-notifications-ignored',
        session_id: row.id,
        ignored,
      });
      setMuted(current => ({ ...current, [row.id]: ignored }));
    },
    [dispatchAction],
  );

  return (
    <div className="mx-auto flex w-full max-w-3xl min-w-0 flex-col gap-4 p-4 pb-8">
      <header className="flex min-w-0 flex-col gap-3">
        <div className="flex min-w-0 flex-wrap items-start justify-between gap-2">
          <div className="min-w-0">
            <h1 className="text-xl font-semibold text-fg">Notifications</h1>
            <p className="mt-0.5 text-sm text-muted">
              {items.length === 0
                ? 'Nothing needs you right now.'
                : `${items.length} awaiting you`}
            </p>
          </div>
          <button
            type="button"
            onClick={() => void toggleAway()}
            disabled={afkPending !== null}
            aria-pressed={away}
            className={`min-h-11 rounded-lg border px-4 text-sm font-semibold disabled:opacity-60 ${
              away
                ? 'border-warn/60 bg-warn/15 text-warn'
                : 'border-line bg-raised text-fg'
            }`}
          >
            {afkPending === true
              ? 'Going AFK…'
              : afkPending === false
                ? 'Ending AFK…'
                : away
                  ? 'End AFK'
                  : 'Go AFK'}
          </button>
        </div>
        {away && (
          <p role="status" className="rounded-lg border border-warn/40 bg-warn/10 px-3 py-2 text-[13px] text-fg">
            Operator away is on. New questions are skipped once so the agent can keep moving; they
            still wait here for you.
          </p>
        )}
        {afkError && (
          <p role="alert" className="rounded-lg border border-danger/40 bg-danger/10 px-3 py-2 text-[13px] text-danger">
            {afkError}
          </p>
        )}
        <PushBanner />
      </header>

      {items.length === 0 ? (
        <section className="flex flex-col items-center gap-3 rounded-2xl border border-line bg-surface px-6 py-10 text-center">
          <CheckCircle2 className="size-9 text-ok" aria-hidden />
          <h2 className="text-lg font-semibold text-fg">All caught up</h2>
          <p className="max-w-sm text-sm text-muted">
            Nothing is waiting for you. Start a new session or check on a workspace.
          </p>
          <button
            type="button"
            onClick={() => navigate({ name: 'workspace' })}
            className="min-h-11 rounded-lg bg-accent px-5 text-sm font-semibold text-accent-fg"
          >
            Open Workspaces
          </button>
        </section>
      ) : (
        <ul role="list" className="flex min-w-0 flex-col gap-3">
          {items.map(item => (
            <li key={item.row.id} role="listitem" className="min-w-0">
              <NotificationItem
                item={item}
                workspaceName={item.row.workspace_id ? workspaceNames[item.row.workspace_id] : undefined}
                now={now}
                onDismiss={onDismiss}
                onMute={onMute}
              />
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
