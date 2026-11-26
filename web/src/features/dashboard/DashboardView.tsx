// Workspaces: the default all-workspaces dashboard, one workspace, or the
// virtual workspace of a parent's sub-agents / reviewers.

import { ArrowLeft, FolderKanban, Plus, RotateCw, TriangleAlert, X } from 'lucide-react';
import { useEffect, useMemo, useState } from 'react';
import type { SessionRow, ViewerSession } from '../../api/types';
import { selectedWorkspaceId } from '../../stores/selectors';
import { useSessionStore } from '../../stores/useSessionStore';
import { useUiStore } from '../../stores/useUiStore';
import { SessionCard } from './SessionCard';

/** Props the shell passes. Keep this shape when replacing the placeholder. */
export type DashboardViewProps = {
  workspaceId?: string;
  subagentParentId?: string;
  childKind?: 'reviewer';
};

const ATTENTION_RANK: Record<string, number> = {
  needs_input: 0,
  important: 1,
  failed: 2,
  unreachable: 2,
  unread: 3,
  working: 4,
  idle: 5,
  inactive: 6,
  unknown: 7,
};

function sessionTime(row: SessionRow): number {
  return row.last_message_at_ms ?? row.last_activity_at_ms ?? Date.parse(row.updated_at) ?? 0;
}

function sortSessions(rows: SessionRow[]): SessionRow[] {
  return [...rows].sort(
    (left, right) =>
      (ATTENTION_RANK[left.attention] ?? 7) - (ATTENTION_RANK[right.attention] ?? 7) ||
      sessionTime(right) - sessionTime(left) ||
      String(left.title || '').localeCompare(String(right.title || '')) ||
      left.id.localeCompare(right.id),
  );
}

function childIds(parent: ViewerSession | SessionRow | undefined, reviewer: boolean): string[] {
  if (!parent) return [];
  const reviewers = new Set(parent.reviewer_session_ids ?? []);
  return (parent.subagent_session_ids ?? []).filter(id => reviewers.has(id) === reviewer);
}

function workspaceChain(
  parentId: string,
  rows: Record<string, SessionRow>,
): { id: string; row?: SessionRow }[] {
  const chain: { id: string; row?: SessionRow }[] = [];
  const seen = new Set<string>();
  let current: string | undefined = parentId;
  while (current && !seen.has(current)) {
    seen.add(current);
    const row: SessionRow | undefined = rows[current];
    chain.push({ id: current, row });
    current = row?.subagent_parent_id;
  }
  return chain.reverse();
}

export default function DashboardView({ workspaceId, subagentParentId, childKind }: DashboardViewProps) {
  const meta = useSessionStore(state => state.meta);
  const rows = useSessionStore(state => state.sessionsById);
  const navigate = useUiStore(state => state.navigate);
  const [dismissedNotices, setDismissedNotices] = useState<ReadonlySet<number>>(() => new Set());
  const [dismissedFailures, setDismissedFailures] = useState<ReadonlySet<string>>(() => new Set());

  const parent = subagentParentId ? rows[subagentParentId] : undefined;
  const reviewer = childKind === 'reviewer';
  const detailLoading = useSessionStore(state =>
    subagentParentId ? state.detailLoadingIds[subagentParentId] === true : false,
  );
  const detailError = useSessionStore(state =>
    subagentParentId ? state.detailErrors[subagentParentId] : undefined,
  );

  useEffect(() => {
    if (!subagentParentId) return;
    const row = useSessionStore.getState().sessionsById[subagentParentId];
    if (row?.detail === false) {
      void useSessionStore.getState().ensureSessionDetail(subagentParentId).catch(() => {});
    }
  }, [subagentParentId]);

  const workspaces = meta?.workspaces ?? [];
  const workspaceNames = useMemo(() => {
    const names: Record<string, string> = {};
    for (const workspace of workspaces) names[workspace.id] = workspace.name;
    return names;
  }, [workspaces]);

  const actionWorkspace =
    workspaceId ??
    selectedWorkspaceId({ name: 'workspace', workspaceId }, workspaces, id => rows[id]);

  const visibleSessions = useMemo(() => {
    if (subagentParentId) {
      return sortSessions(
        childIds(parent, reviewer)
          .map(id => rows[id])
          .filter((row): row is SessionRow => Boolean(row)),
      );
    }
    return sortSessions(
      Object.values(rows).filter(row => {
        if (row.subagent_parent_id) return false;
        return workspaceId ? row.workspace_id === workspaceId : true;
      }),
    );
  }, [parent, reviewer, rows, subagentParentId, workspaceId]);

  const notices = (meta?.notices ?? []).filter(notice => !dismissedNotices.has(notice.id));
  const launchFailures = (meta?.launch_failures ?? []).filter(
    failure => !dismissedFailures.has(failure.id),
  );

  const chain = subagentParentId ? workspaceChain(subagentParentId, rows) : [];
  const noun = reviewer ? 'reviewers' : 'sub-agents';
  const heading = subagentParentId
    ? reviewer
      ? 'Reviewers'
      : 'Sub-agents'
    : workspaceId
      ? workspaceNames[workspaceId] || 'Workspace'
      : 'Workspaces';

  return (
    <div className="mx-auto flex w-full max-w-6xl min-w-0 flex-col gap-4 p-4 pb-8">
      <header className="flex min-w-0 flex-col gap-3">
        {subagentParentId ? (
          <nav aria-label="Breadcrumb" className="flex min-w-0 flex-wrap items-center gap-1 text-[13px]">
            {chain.map((segment, index) => (
              <span key={segment.id} className="flex min-w-0 items-center gap-1">
                {index > 0 && (
                  <span className="text-muted" aria-hidden>
                    /
                  </span>
                )}
                <button
                  type="button"
                  aria-label={
                    segment.id === subagentParentId
                      ? `Back to ${segment.row?.title || segment.id}`
                      : undefined
                  }
                  onClick={() => navigate({ name: 'conversation', sessionId: segment.id })}
                  className="inline-flex min-h-11 max-w-full items-center gap-1 break-words rounded px-1 text-accent hover:underline"
                >
                  {index === 0 && <ArrowLeft className="size-3.5 shrink-0" aria-hidden />}
                  {segment.row?.title || segment.id}
                </button>
              </span>
            ))}
            <span className="text-muted" aria-hidden>
              /
            </span>
            <span aria-current="page" className="break-words font-medium text-fg">
              {reviewer ? 'Reviewers' : 'Sub-agents'}
            </span>
          </nav>
        ) : (
          <>
            <div className="flex min-w-0 flex-wrap items-start justify-between gap-2">
              <div className="min-w-0">
                <h1 className="break-words text-xl font-semibold text-fg">{heading}</h1>
                <p className="mt-0.5 text-sm text-muted">
                  {visibleSessions.length} session{visibleSessions.length === 1 ? '' : 's'}
                </p>
              </div>
              <div className="flex flex-wrap items-center gap-2">
                <a
                  href={actionWorkspace ? `#workspace/${actionWorkspace}/new` : undefined}
                  aria-disabled={!actionWorkspace}
                  onClick={event => {
                    event.preventDefault();
                    if (actionWorkspace) navigate({ name: 'new', workspaceId: actionWorkspace });
                  }}
                  className={`inline-flex min-h-11 items-center gap-1.5 rounded-lg bg-accent px-4 text-sm font-semibold text-accent-fg ${
                    actionWorkspace ? '' : 'pointer-events-none opacity-50'
                  }`}
                >
                  <Plus className="size-4" aria-hidden />
                  New session
                </a>
                <a
                  href={actionWorkspace ? `#workspace/${actionWorkspace}/resume` : undefined}
                  aria-disabled={!actionWorkspace}
                  onClick={event => {
                    event.preventDefault();
                    if (actionWorkspace) navigate({ name: 'resume', workspaceId: actionWorkspace });
                  }}
                  className={`inline-flex min-h-11 items-center gap-1.5 rounded-lg border border-line bg-raised px-4 text-sm font-semibold text-fg ${
                    actionWorkspace ? '' : 'pointer-events-none opacity-50'
                  }`}
                >
                  <RotateCw className="size-4" aria-hidden />
                  Resume
                </a>
              </div>
            </div>
            <nav aria-label="Workspaces" className="flex min-w-0 flex-wrap items-center gap-2">
              <button
                type="button"
                aria-pressed={!workspaceId}
                onClick={() => navigate({ name: 'workspace' })}
                className={`min-h-11 rounded-lg border px-3 text-sm font-medium ${
                  !workspaceId
                    ? 'border-accent bg-accent/15 text-accent'
                    : 'border-line bg-raised text-fg'
                }`}
              >
                All workspaces
              </button>
              {workspaces.map(workspace => (
                <button
                  key={workspace.id}
                  type="button"
                  aria-pressed={workspaceId === workspace.id}
                  onClick={() => navigate({ name: 'workspace', workspaceId: workspace.id })}
                  className={`min-h-11 max-w-full break-words rounded-lg border px-3 text-sm font-medium ${
                    workspaceId === workspace.id
                      ? 'border-accent bg-accent/15 text-accent'
                      : 'border-line bg-raised text-fg'
                  }`}
                >
                  {workspace.name}
                </button>
              ))}
            </nav>
          </>
        )}
      </header>

      {notices.length > 0 && (
        <section aria-label="Notices" className="flex min-w-0 flex-col gap-2">
          {notices.map(notice => (
            <div
              key={notice.id}
              role="status"
              className="flex min-w-0 items-start gap-2 rounded-xl border border-warn/50 bg-warn/10 p-3"
            >
              <TriangleAlert className="mt-0.5 size-4 shrink-0 text-warn" aria-hidden />
              <p className="min-w-0 flex-1 break-words text-[13px] text-fg">{notice.text}</p>
              <button
                type="button"
                aria-label="Dismiss notice"
                onClick={() =>
                  setDismissedNotices(current => new Set([...current, notice.id]))
                }
                className="inline-flex size-11 shrink-0 items-center justify-center rounded text-muted hover:text-fg"
              >
                <X className="size-4" aria-hidden />
              </button>
            </div>
          ))}
        </section>
      )}

      {launchFailures.length > 0 && (
        <section aria-label="Launch failures" className="flex min-w-0 flex-col gap-2">
          {launchFailures.map(failure => (
            <div
              key={failure.id}
              role="alert"
              className="flex min-w-0 items-start gap-2 rounded-xl border border-danger/50 bg-danger/10 p-3"
            >
              <TriangleAlert className="mt-0.5 size-4 shrink-0 text-danger" aria-hidden />
              <div className="min-w-0 flex-1">
                <p className="break-words text-[13px] font-semibold text-fg">
                  A session could not be started
                </p>
                <p className="mt-0.5 break-words text-[13px] text-fg">
                  {failure.error ||
                    'Check the project and target, then retry. Details are in the daemon logs.'}
                </p>
              </div>
              {failure.session_id && rows[failure.session_id] && (
                <button
                  type="button"
                  onClick={() =>
                    navigate({ name: 'conversation', sessionId: failure.session_id! })
                  }
                  className="min-h-11 shrink-0 rounded-lg border border-line bg-surface px-3 text-xs font-semibold text-fg"
                >
                  Open
                </button>
              )}
              <button
                type="button"
                aria-label="Dismiss launch failure"
                onClick={() =>
                  setDismissedFailures(current => new Set([...current, failure.id]))
                }
                className="inline-flex size-11 shrink-0 items-center justify-center rounded text-muted hover:text-fg"
              >
                <X className="size-4" aria-hidden />
              </button>
            </div>
          ))}
        </section>
      )}

      {subagentParentId && !parent && (
        <p role="alert" className="rounded-xl border border-danger/50 bg-danger/10 p-3 text-sm text-danger">
          This parent session is no longer available.
        </p>
      )}
      {subagentParentId && parent && (detailLoading || detailError) && visibleSessions.length === 0 && (
        <p className="text-sm text-muted">
          {detailError ? `Could not load the parent session: ${detailError}` : 'Loading sessions…'}
        </p>
      )}

      {visibleSessions.length === 0 ? (
        <section className="flex flex-col items-center gap-3 rounded-2xl border border-line bg-surface px-6 py-10 text-center">
          <FolderKanban className="size-9 text-muted" aria-hidden />
          <h2 className="text-lg font-semibold text-fg">
            {subagentParentId ? `No ${noun} yet` : 'No sessions here yet'}
          </h2>
          <p className="max-w-sm text-sm text-muted">
            {subagentParentId
              ? 'When this session delegates work, its children appear here.'
              : 'Start a new session in this workspace to see it here.'}
          </p>
        </section>
      ) : (
        <div className="grid min-w-0 grid-cols-1 gap-3 lg:grid-cols-2 xl:grid-cols-3">
          {visibleSessions.map(session => (
            <SessionCard
              key={session.id}
              session={session}
              workspaceName={
                session.workspace_id ? workspaceNames[session.workspace_id] : undefined
              }
            />
          ))}
        </div>
      )}
    </div>
  );
}
