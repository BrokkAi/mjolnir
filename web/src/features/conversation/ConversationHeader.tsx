import { useCallback, useEffect, useState, type ReactNode } from 'react';
import {
  ChevronLeft,
  Eye,
  FileDiff,
  FolderInput,
  Info,
  MoreHorizontal,
  MoveRight,
  PauseCircle,
  Pencil,
  RefreshCw,
  RotateCcw,
  SlidersHorizontal,
  Trash2,
  Users,
} from 'lucide-react';
import type { ControllerAction, SessionRow } from '../../api/types';
import { HeaderActions } from '../../components/shell/HeaderActions';
import { useSessionStore } from '../../stores/useSessionStore';
import { useTranscriptStore } from '../../stores/useTranscriptStore';
import { useUiStore, type Route } from '../../stores/useUiStore';
import { useLifecycleModal } from '../lifecycle/useLifecycleModal';
import { describeError } from './format';

/**
 * The conversation title bar. On phones the action buttons portal into the
 * shell's header, so a conversation never grows a second title bar; on desktop
 * this compact strip is the only header and renders the same buttons inline.
 */

export interface ConversationHeaderProps {
  session: SessionRow;
  sessionId: string;
  subagentParentId?: string;
  childKind?: 'reviewer';
  onOpenSubagents: () => void;
  onOpenDiff: () => void;
  onOpenInfo: () => void;
}

const TRANSITION_LABEL: Record<string, string> = {
  starting: 'Starting',
  resuming: 'Resuming',
  moving: 'Moving',
  suspending: 'Suspending',
  destroying: 'Destroying',
  stopping: 'Stopping',
};

function statusOf(session: SessionRow): { label: string; dot: string } {
  const transition = session.verdict.transition;
  if (transition) return { label: TRANSITION_LABEL[transition] ?? transition, dot: 'bg-warn' };
  if (session.transitioning) return { label: 'Transitioning', dot: 'bg-warn' };
  if (session.verdict.working) return { label: 'Working', dot: 'bg-ok animate-pulse' };
  if (session.attention === 'failed' || session.attention === 'unreachable') {
    return { label: 'Failed', dot: 'bg-danger' };
  }
  if (session.lifecycle === 'suspended') return { label: 'Suspended', dot: 'bg-muted' };
  if (session.lifecycle === 'live') return { label: 'Live', dot: 'bg-ok' };
  return { label: session.lifecycle, dot: 'bg-muted' };
}

function IconButton({
  label,
  onClick,
  expanded,
  children,
}: {
  label: string;
  onClick: () => void;
  expanded?: boolean;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      aria-label={label}
      title={label}
      aria-haspopup={expanded === undefined ? undefined : 'menu'}
      aria-expanded={expanded}
      onClick={onClick}
      className="relative grid size-10 place-items-center rounded-lg text-fg hover:bg-raised active:bg-raised"
    >
      {children}
    </button>
  );
}

interface MenuAction {
  id: string;
  label: string;
  icon: ReactNode;
  onSelect: () => void;
  danger?: boolean;
  separatorBefore?: boolean;
}

function OverflowMenu({ items }: { items: MenuAction[] }) {
  const [open, setOpen] = useState(false);

  useEffect(() => {
    if (!open) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== 'Escape' || event.defaultPrevented) return;
      event.preventDefault();
      setOpen(false);
    };
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, [open]);

  return (
    <>
      <IconButton label="More actions" expanded={open} onClick={() => setOpen(value => !value)}>
        <MoreHorizontal className="size-5" aria-hidden />
      </IconButton>
      {open && (
        <>
          <div
            className="fixed inset-0 z-40"
            aria-hidden="true"
            onClick={() => setOpen(false)}
          />
          <div
            role="menu"
            aria-label="Session actions"
            className="fixed right-2 top-14 z-50 w-60 overflow-hidden rounded-xl border border-line bg-surface p-1 shadow-2xl"
          >
            {items.map(item => (
              <div key={item.id}>
                {item.separatorBefore && <div className="my-1 border-t border-line" />}
                <button
                  type="button"
                  role="menuitem"
                  onClick={() => {
                    setOpen(false);
                    item.onSelect();
                  }}
                  className={`flex min-h-9 w-full items-center gap-2 rounded-lg px-2.5 py-1.5 text-left text-sm hover:bg-raised ${
                    item.danger ? 'text-danger' : 'text-fg'
                  }`}
                >
                  <span className="grid size-4 place-items-center" aria-hidden>
                    {item.icon}
                  </span>
                  <span className="min-w-0 flex-1 truncate">{item.label}</span>
                </button>
              </div>
            ))}
          </div>
        </>
      )}
    </>
  );
}

export function ConversationHeader({
  session,
  sessionId,
  subagentParentId,
  childKind,
  onOpenSubagents,
  onOpenDiff,
  onOpenInfo,
}: ConversationHeaderProps) {
  const parent = useSessionStore(state => (subagentParentId ? state.sessionsById[subagentParentId] : undefined));
  const status = statusOf(session);
  const capabilities = session.capabilities;
  const transitioning = Boolean(session.transitioning || session.verdict.transition);
  const nativeSubagentCount = session.detail === false ? 0 : (session.native_subagents?.length ?? 0);
  const activeSubagentCount = session.verdict?.working_children ?? 0;
  const hasAnySubagents =
    activeSubagentCount > 0 ||
    (session.subagent_session_ids?.length ?? 0) +
    (session.reviewer_session_ids?.length ?? 0) +
    nativeSubagentCount > 0;
  const detail =
    session.activity_details?.label || session.question_message || session.activity || '';

  const navigate = useCallback((route: Route) => useUiStore.getState().navigate(route), []);

  const runAction = useCallback(
    (action: ControllerAction, failure: string) => {
      void useSessionStore
        .getState()
        .dispatchAction(action)
        .catch(error => useUiStore.getState().pushToast('error', `${failure}: ${describeError(error)}`));
    },
    [],
  );

  const refresh = useCallback(() => {
    const store = useSessionStore.getState();
    if (store.sessionsById[sessionId]?.detail === false) {
      void store
        .ensureSessionDetail(sessionId)
        .catch(error => useUiStore.getState().pushToast('error', `Could not refresh: ${describeError(error)}`));
    }
    void useTranscriptStore.getState().syncConversation(sessionId, { force: true });
  }, [sessionId]);

  const openModal = useCallback(
    (kind: 'rename' | 'change-workspace' | 'container-settings' | 'suspend' | 'destroy') =>
      useLifecycleModal.getState().openModal({ kind, sessionId }),
    [sessionId],
  );

  const actions: MenuAction[] = [
    {
      id: 'review',
      label: 'Start review',
      icon: <Eye className="size-4" />,
      onSelect: () => runAction({ action: 'start-review', session_id: sessionId }, 'Could not start a review'),
    },
    {
      id: 'refresh',
      label: 'Refresh',
      icon: <RefreshCw className="size-4" />,
      onSelect: refresh,
    },
    {
      id: 'info',
      label: 'Session info',
      icon: <Info className="size-4" />,
      onSelect: onOpenInfo,
    },
    {
      id: 'move',
      label: 'Move session',
      icon: <MoveRight className="size-4" />,
      separatorBefore: true,
      onSelect: () => navigate({ name: 'move', workspaceId: session.workspace_id ?? '', sessionId }),
    },
    {
      id: 'rename',
      label: 'Rename',
      icon: <Pencil className="size-4" />,
      onSelect: () => openModal('rename'),
    },
    {
      id: 'workspace',
      label: 'Change workspace',
      icon: <FolderInput className="size-4" />,
      onSelect: () => openModal('change-workspace'),
    },
    {
      id: 'container',
      label: 'Container settings',
      icon: <SlidersHorizontal className="size-4" />,
      onSelect: () => openModal('container-settings'),
    },
    {
      id: 'restart',
      label: 'Restart',
      icon: <RotateCcw className="size-4" />,
      separatorBefore: true,
      onSelect: () => runAction({ action: 'restart', session_id: sessionId }, 'Could not restart'),
    },
    {
      id: 'suspend',
      label: 'Suspend',
      icon: <PauseCircle className="size-4" />,
      onSelect: () => openModal('suspend'),
    },
    {
      id: 'destroy',
      label: 'Destroy',
      icon: <Trash2 className="size-4" />,
      danger: true,
      onSelect: () => openModal('destroy'),
    },
  ];

  // Capability gates come from the server; the browser never re-derives them.
  const visible = actions.filter(item => {
    switch (item.id) {
      case 'refresh':
      case 'info':
        return true;
      case 'review':
        return capabilities.open && !transitioning;
      case 'move':
        return (
          Boolean(capabilities.move_session) &&
          session.verdict.move_session === 'ready' &&
          Boolean(session.workspace_id)
        );
      case 'rename':
        return Boolean(capabilities.rename);
      case 'workspace':
        return (
          Boolean(capabilities.change_workspace) &&
          session.verdict.change_workspace === 'ready'
        );
      case 'container':
        return Boolean(capabilities.container_settings) && session.lifecycle === 'live';
      case 'restart':
        return Boolean(capabilities.restart) && session.verdict.restart === 'ready';
      case 'suspend':
        return Boolean(capabilities.suspend) && session.lifecycle === 'live';
      case 'destroy':
        return Boolean(capabilities.destroy);
      default:
        return false;
    }
  });

  const buttons = (
    <>
      {hasAnySubagents && (
        <IconButton label={activeSubagentCount > 0 ? `Sub-agents and reviewers, ${activeSubagentCount} active` : 'Sub-agents and reviewers'} onClick={onOpenSubagents}>
          <Users className="size-5" aria-hidden />
          {activeSubagentCount > 0 && <span className="absolute -right-0.5 -top-0.5 rounded-full bg-accent px-1 text-[10px] font-semibold leading-4 text-accent-fg">
            {activeSubagentCount}
          </span>}
        </IconButton>
      )}
      <IconButton label="Session diff" onClick={onOpenDiff}>
        <FileDiff className="size-5" aria-hidden />
      </IconButton>
      <OverflowMenu items={visible} />
    </>
  );

  return (
    <>
      {subagentParentId && (
        <nav
          aria-label="Parent conversation"
          className="flex shrink-0 items-center border-b border-line bg-surface px-2 py-1 md:hidden"
        >
          <button
            type="button"
            onClick={() => navigate({ name: 'conversation', sessionId: subagentParentId })}
            className="inline-flex min-h-7 min-w-0 items-center gap-1 rounded-lg px-1.5 text-[11.5px] text-muted hover:bg-raised hover:text-fg"
          >
            <ChevronLeft className="size-3.5 shrink-0" aria-hidden />
            <Users className="size-3.5 shrink-0" aria-hidden />
            <span className="max-w-60 truncate">
              {parent?.title || (childKind === 'reviewer' ? 'Reviewers' : 'Sub-agents')}
            </span>
          </button>
        </nav>
      )}
      <header className="relative hidden shrink-0 items-center gap-2 border-b border-line bg-surface px-2 py-1.5 md:flex">
        {subagentParentId && (
          <>
            <button
              type="button"
              onClick={() => navigate({ name: 'conversation', sessionId: subagentParentId })}
              className="inline-flex min-h-8 min-w-0 items-center gap-1 rounded-lg px-1.5 text-xs text-muted hover:bg-raised hover:text-fg"
              aria-label={`Back to parent conversation${parent?.title ? `: ${parent.title}` : ''}`}
            >
              <ChevronLeft className="size-3.5 shrink-0" aria-hidden />
              <Users className="size-3.5 shrink-0" aria-hidden />
              <span className="max-w-48 truncate">{parent?.title || (childKind === 'reviewer' ? 'Reviewers' : 'Sub-agents')}</span>
            </button>
            <ChevronLeft className="size-3.5 shrink-0 rotate-180 text-muted" aria-hidden />
          </>
        )}
        <div className="min-w-0 flex-1">
          <div className="flex min-w-0 items-center gap-2">
            <h2 className="min-w-0 truncate text-[13.5px] font-semibold tracking-tight text-fg">
              {session.title || sessionId}
            </h2>
            <span className="inline-flex shrink-0 items-center gap-1.5 rounded-full border border-line px-2 py-0.5 text-[10.5px] text-muted">
              <span className={`size-1.5 rounded-full ${status.dot}`} aria-hidden />
              {status.label}
            </span>
            {childKind === 'reviewer' && (
              <span className="shrink-0 rounded-full bg-raised px-2 py-0.5 text-[10.5px] text-muted">Reviewer</span>
            )}
          </div>
          {detail && (
            <p className="mt-0.5 truncate text-[11px] text-muted" title={detail}>
              {detail}
            </p>
          )}
        </div>
        <div className="flex shrink-0 items-center gap-1">
          <HeaderActions>{buttons}</HeaderActions>
        </div>
      </header>
    </>
  );
}
