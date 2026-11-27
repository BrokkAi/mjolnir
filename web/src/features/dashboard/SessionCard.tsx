// One session card on the Workspaces dashboard. Every mutating control here
// appears because a capability the daemon published says it may; nothing is
// inferred from a status string.

import {
  Bot,
  ChevronRight,
  Ellipsis,
  MessageSquare,
  Users,
} from 'lucide-react';
import { useEffect, useMemo, useRef, useState } from 'react';
import type { SessionRow } from '../../api/types';
import { useLifecycleModal } from '../lifecycle/useLifecycleModal';
import { useSessionStore } from '../../stores/useSessionStore';
import { useUiStore } from '../../stores/useUiStore';
import { notificationsIgnored, openRouteFor } from '../notifications/notificationsLogic';

const OPERATION_LABELS: Record<string, string> = {
  create: 'Starting',
  resume: 'Resuming',
  move: 'Moving',
  suspend: 'Suspending',
  destroy: 'Destroying',
  stop: 'Stopping',
  cleanup: 'Cleaning up',
  checkpoint: 'Checkpointing',
};

const TRANSITION_LABELS: Record<string, string> = {
  starting: 'Starting',
  resuming: 'Resuming',
  moving: 'Moving',
  suspending: 'Suspending',
  destroying: 'Destroying',
  stopping: 'Stopping',
};

export function formatElapsed(milliseconds: number): string {
  const seconds = Math.max(0, Math.floor(milliseconds / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m${String(seconds % 60).padStart(2, '0')}s`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h${String(minutes % 60).padStart(2, '0')}m`;
  const days = Math.floor(hours / 24);
  return `${days}d${String(hours % 24).padStart(2, '0')}h`;
}

/** The one activity line: lifecycle progress, recovery states, then chat phase. */
export function sessionActivityLabel(row: SessionRow, now = Date.now()): string {
  const operation = row.operation;
  if (operation) {
    const stages = [...(operation.stages ?? [])]
      .filter(stage => stage.label)
      .sort(
        (left, right) =>
          (left.started_at_epoch_seconds ?? 0) - (right.started_at_epoch_seconds ?? 0),
      );
    const started =
      stages[0]?.started_at_epoch_seconds ?? operation.started_at_epoch_seconds;
    const label =
      stages.map(stage => stage.label).join(' · ') ||
      OPERATION_LABELS[operation.kind] ||
      operation.kind;
    const elapsed =
      started == null ? '' : ` ${formatElapsed(now - started * 1000)}`;
    return `${label}${elapsed}`;
  }
  if (row.launch_error) return row.launch_error;
  if (row.configuration_issue) return 'Needs configuration repair';
  if (row.storage_problem) return row.storage_problem;
  if (row.move_recovery?.failure_notice) return row.move_recovery.failure_notice;
  if (row.has_error && row.lifecycle === 'failed') return 'Launch failed';
  const transition = row.verdict?.transition;
  if (transition) return TRANSITION_LABELS[transition] ?? transition;
  if (row.lifecycle === 'starting') return 'Starting';
  if (row.lifecycle === 'suspending') return 'Suspending';
  if (row.lifecycle === 'failed') return 'Failed';
  if (row.quota_recovery) {
    return row.quota_recovery.retry_at_ms == null
      ? 'Quota limit · reset time unknown'
      : `Quota limit · resumes ${new Date(row.quota_recovery.retry_at_ms).toLocaleString()}`;
  }
  if (row.capacity_retry) {
    const seconds = Math.max(0, Math.ceil((row.capacity_retry.retry_at_ms - now) / 1000));
    return `Provider unavailable · retrying in ${formatElapsed(seconds * 1000)}`;
  }
  if (row.retry_assessment_pending) return 'Checking response';
  if (row.chat_phase === 'running') return 'Working';
  if (row.chat_phase === 'closing') return 'Closing';
  if (row.chat_phase === 'closed') return 'Closed';
  if (row.activity) return row.activity;
  if (row.attention === 'needs_input') return 'Waiting for your input';
  if (row.attention === 'important') return 'Important update';
  if (row.attention === 'failed' || row.attention === 'unreachable') return 'Needs recovery';
  if (row.verdict?.working) return 'Working';
  if (row.state === 'parked') return 'Parked';
  if (row.is_idle) return 'Idle';
  if (row.lifecycle === 'suspended') return 'Suspended';
  return row.lifecycle === 'live' ? 'Live' : row.lifecycle;
}

export function statusTone(row: SessionRow): 'working' | 'waiting' | 'important' | 'failed' | 'unread' | 'idle' {
  if (row.lifecycle === 'failed' || row.has_error || row.launch_error || row.attention === 'failed' || row.attention === 'unreachable') {
    return 'failed';
  }
  if (row.attention === 'needs_input' || pendingCount(row) > 0) {
    return 'waiting';
  }
  if (row.attention === 'important') return 'important';
  if (row.verdict?.working || row.verdict?.transition || row.chat_phase === 'running' || row.lifecycle === 'starting') {
    return 'working';
  }
  if (row.attention === 'unread') return 'unread';
  return 'idle';
}

const DOT_TONE: Record<ReturnType<typeof statusTone>, string> = {
  working: 'bg-ok',
  waiting: 'bg-warn',
  important: 'bg-accent',
  failed: 'bg-danger',
  unread: 'bg-accent',
  idle: 'bg-muted',
};

function attentionBadge(row: SessionRow): { label: string; className: string } | null {
  if (notificationsIgnored(row)) return null;
  if (row.attention === 'needs_input' || row.notification_card?.badge === 'needs_input') {
    return { label: 'Needs input', className: 'border-warn/50 bg-warn/10 text-warn' };
  }
  if (row.attention === 'important' || row.notification_card?.badge === 'important') {
    return { label: 'Important', className: 'border-accent/50 bg-accent/10 text-accent' };
  }
  if (row.attention === 'failed' || row.attention === 'unreachable') {
    return { label: 'Failed', className: 'border-danger/50 bg-danger/10 text-danger' };
  }
  return null;
}

function pendingCount(row: SessionRow): number {
  return row.detail === false
    ? row.pending_elicitation_count
    : (row.pending_elicitations ?? []).length;
}

function queuedCount(row: SessionRow): number {
  return row.detail === false ? row.queued_prompt_count : (row.queued_prompts ?? []).length;
}

export interface SessionCardProps {
  session: SessionRow;
  workspaceName?: string;
}

export function SessionCard({ session, workspaceName }: SessionCardProps) {
  const navigate = useUiStore(state => state.navigate);
  const dispatchAction = useSessionStore(state => state.dispatchAction);
  const [menuOpen, setMenuOpen] = useState(false);
  const [pending, setPending] = useState<string | null>(null);
  const wrapper = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!menuOpen) return;
    const onPointerDown = (event: PointerEvent) => {
      if (!wrapper.current?.contains(event.target as Node)) setMenuOpen(false);
    };
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') setMenuOpen(false);
    };
    document.addEventListener('pointerdown', onPointerDown, true);
    document.addEventListener('keydown', onKeyDown);
    return () => {
      document.removeEventListener('pointerdown', onPointerDown, true);
      document.removeEventListener('keydown', onKeyDown);
    };
  }, [menuOpen]);

  const tone = statusTone(session);
  const badge = attentionBadge(session);
  const reviewerIds = session.reviewer_session_ids ?? [];
  const reviewerSet = new Set(reviewerIds);
  const hasWorkers = (session.subagent_session_ids ?? []).some(id => !reviewerSet.has(id));
  const workers = session.verdict?.working_descendants ?? session.verdict?.working_children ?? 0;
  const reviewers = useSessionStore(state => {
    let present = false;
    let active = 0;
    for (const id of reviewerIds) {
      const row = state.sessionsById[id];
      if (!row) continue;
      present = true;
      if (row.verdict?.working || row.active) active += 1;
    }
    return present ? active : (session.verdict?.review_working ? reviewerIds.length : 0);
  });
  const questions = pendingCount(session);
  const queued = queuedCount(session);
  const activity = sessionActivityLabel(session);

  const can = session.capabilities ?? {};
  const openDestination = openRouteFor(session);
  const canOpen =
    can.open === true ||
    can.read_history === true ||
    (openDestination === 'resume' && can.resume === true);
  const actions = useMemo(() => {
    const list: { id: string; label: string; danger?: boolean; run: () => Promise<void> | void }[] = [];
    if (canOpen) {
      list.push({
        id: 'open',
        label: openDestination === 'resume' ? 'Resume session' : 'Open conversation',
        run: () => navigateSession(),
      });
    }
    if (hasWorkers && workers === 0) {
      list.push({ id: 'subagents', label: 'Sub-agents', run: () => navigate({ name: 'workspace', subagentParentId: session.id }) });
    }
    if (reviewerIds.length > 0 && reviewers === 0) {
      list.push({ id: 'reviewers', label: 'Reviewers', run: () => navigate({ name: 'workspace', subagentParentId: session.id, childKind: 'reviewer' }) });
    }
    if (can.move_session === true) {
      list.push({
        id: 'move',
        label: 'Move…',
        run: () =>
          navigate({
            name: 'move',
            workspaceId: session.workspace_id ?? '',
            sessionId: session.id,
          }),
      });
    }
    if (can.rename === true) {
      list.push({
        id: 'rename',
        label: 'Rename…',
        run: () => useLifecycleModal.getState().openModal({ kind: 'rename', sessionId: session.id }),
      });
    }
    if (can.change_workspace === true) {
      list.push({
        id: 'change_workspace',
        label: 'Change workspace…',
        run: () =>
          useLifecycleModal.getState().openModal({
            kind: 'change-workspace',
            sessionId: session.id,
          }),
      });
    }
    if (can.container_settings === true) {
      list.push({
        id: 'container_settings',
        label: 'Container settings…',
        run: () =>
          useLifecycleModal.getState().openModal({
            kind: 'container-settings',
            sessionId: session.id,
          }),
      });
    }
    if (can.suspend === true) {
      list.push({
        id: 'suspend',
        label: 'Suspend…',
        run: () => useLifecycleModal.getState().openModal({ kind: 'suspend', sessionId: session.id }),
      });
    }
    if (can.restart === true) {
      list.push({
        id: 'restart',
        label: 'Restart agent',
        run: async () => {
          setPending('restart');
          try {
            await dispatchAction({ action: 'restart', session_id: session.id });
          } catch (error) {
            useUiStore
              .getState()
              .pushToast('error', `Could not restart ${session.title || session.id}: ${error instanceof Error ? error.message : String(error)}`);
          } finally {
            setPending(null);
          }
        },
      });
    }
    if (can.destroy === true) {
      list.push({
        id: 'destroy',
        label: 'Destroy…',
        danger: true,
        run: () => useLifecycleModal.getState().openModal({ kind: 'destroy', sessionId: session.id }),
      });
    }
    return list;

    function navigateSession() {
      if (openDestination === 'resume') {
        navigate({
          name: 'resume',
          workspaceId: session.workspace_id ?? '',
          sessionId: session.id,
        });
      } else {
        navigate({ name: 'conversation', sessionId: session.id });
      }
    }
  }, [can.change_workspace, can.container_settings, can.destroy, can.move_session, can.rename, can.restart, can.suspend, canOpen, dispatchAction, hasWorkers, navigate, openDestination, reviewerIds.length, reviewers, session.id, session.title, session.workspace_id, workers]);

  const open = () => {
    if (openDestination === 'resume') {
      navigate({ name: 'resume', workspaceId: session.workspace_id ?? '', sessionId: session.id });
    } else {
      navigate({ name: 'conversation', sessionId: session.id });
    }
  };

  const runAction = (run: () => Promise<void> | void) => {
    setMenuOpen(false);
    void run();
  };

  return (
    <article
      data-testid={`session-card-${session.id}`}
      className="relative flex min-w-0 flex-col gap-2 rounded-xl border border-line bg-surface p-3 shadow-sm sm:p-4"
    >
      <div className="flex min-w-0 items-start gap-1">
        <button
          type="button"
          onClick={open}
          disabled={!canOpen}
          className="min-h-11 min-w-0 flex-1 rounded text-left disabled:cursor-default"
        >
          <h3 className="break-words text-[15px] font-semibold leading-snug text-fg">
            {session.title || session.id}
          </h3>
        </button>
        {actions.length > 0 && (
          <div ref={wrapper} className="relative shrink-0">
            <button
              type="button"
              aria-label={`Actions for ${session.title || session.id}`}
              aria-haspopup="menu"
              aria-expanded={menuOpen}
              onClick={() => setMenuOpen(value => !value)}
              className="inline-flex size-11 items-center justify-center rounded-lg text-muted hover:bg-raised hover:text-fg"
            >
              <Ellipsis className="size-5" aria-hidden />
            </button>
            {menuOpen && (
              <div
                role="menu"
                className="absolute right-0 top-full z-30 mt-1 w-60 max-w-[80vw] rounded-xl border border-line bg-surface p-1 shadow-xl"
              >
                {actions.map(action => (
                  <button
                    key={action.id}
                    type="button"
                    role="menuitem"
                    onClick={() => runAction(action.run)}
                    disabled={pending !== null}
                    className={`flex min-h-11 w-full items-center rounded-lg px-3 text-left text-sm font-medium hover:bg-raised disabled:opacity-50 ${
                      action.danger ? 'text-danger' : 'text-fg'
                    }`}
                  >
                    {action.label}
                  </button>
                ))}
              </div>
            )}
          </div>
        )}
      </div>

      <div className="flex min-w-0 flex-wrap items-center gap-2">
        <span
          aria-label={`Status: ${activity}`}
          className={`size-2.5 shrink-0 rounded-full ${DOT_TONE[tone]} ${tone === 'working' ? 'animate-pulse' : ''}`}
        />
        <span className="min-w-0 flex-1 basis-[12rem] break-words text-[13px] text-fg">
          {activity}
        </span>
        {badge && (
          <span
            className={`shrink-0 rounded-full border px-2 py-0.5 text-[11px] font-semibold uppercase tracking-wide ${badge.className}`}
          >
            {badge.label}
          </span>
        )}
      </div>

      {(questions > 0 || queued > 0 || workers > 0 || reviewers > 0) && (
        <div className="flex min-w-0 flex-wrap items-center gap-1.5">
          {questions > 0 && (
            <span
              data-testid="elicitation-pill"
              className="inline-flex min-h-8 items-center gap-1 rounded-full border border-warn/50 bg-warn/10 px-2 text-[12px] font-semibold text-warn"
            >
              <MessageSquare className="size-3.5" aria-hidden />
              Question waiting
            </span>
          )}
          {queued > 0 && (
            <span className="rounded-full border border-line bg-raised px-2 py-1 text-[12px] font-medium text-fg">
              {queued} queued
            </span>
          )}
          {workers > 0 && (
            <button
              type="button"
              onClick={() => navigate({ name: 'workspace', subagentParentId: session.id })}
              className="inline-flex min-h-11 items-center gap-1 rounded-full border border-line bg-raised px-2.5 text-[12px] font-medium text-fg hover:border-accent"
            >
              <Bot className="size-3.5" aria-hidden />
              {workers} sub-agent{workers === 1 ? '' : 's'}
              <ChevronRight className="size-3" aria-hidden />
            </button>
          )}
          {reviewers > 0 && (
            <button
              type="button"
              onClick={() =>
                navigate({ name: 'workspace', subagentParentId: session.id, childKind: 'reviewer' })
              }
              className="inline-flex min-h-11 items-center gap-1 rounded-full border border-line bg-raised px-2.5 text-[12px] font-medium text-fg hover:border-accent"
            >
              <Users className="size-3.5" aria-hidden />
              {reviewers} reviewer{reviewers === 1 ? '' : 's'}
              <ChevronRight className="size-3" aria-hidden />
            </button>
          )}
        </div>
      )}

      <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 text-[12px] text-muted">
        {workspaceName && (
          <span className="min-w-0 break-words rounded border border-line bg-raised px-1.5 py-0.5">
            {workspaceName}
          </span>
        )}
        {session.project_label && (
          <span className="min-w-0 break-words rounded border border-line bg-raised px-1.5 py-0.5">
            {session.project_label}
          </span>
        )}
        {session.target_id && (
          <span className="min-w-0 break-words rounded border border-line bg-raised px-1.5 py-0.5">
            {session.target_id}
          </span>
        )}
        {session.profile_id && (
          <span className="min-w-0 break-words rounded border border-line bg-raised px-1.5 py-0.5">
            {session.profile_id}
          </span>
        )}
        {session.display_location && (
          <span className="min-w-0 break-words">{session.display_location}</span>
        )}
      </div>
    </article>
  );
}
