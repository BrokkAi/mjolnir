// Derived facts shared by the shell and views. One owner per question: the
// views ask these functions instead of recomputing from rows.

import type { SessionRow, ViewerLaunchFailure, ViewerWorkspace } from '../api/types';
import type { Route } from './useUiStore';

/** A session that is itself an agent child of another session. */
export const isSubagentSession = (row: SessionRow): boolean => Boolean(row.subagent_parent_id);

/**
 * Sessions that need the person: a published notification card, or a failed
 * or unreachable session that needs a person. Sessions the person muted are
 * excluded. This is what decides the default screen; the Notifications view
 * owns the richer ordering and dismissal handling.
 */
export function attentionSessions(rows: Iterable<SessionRow>): SessionRow[] {
  const result: SessionRow[] = [];
  for (const row of rows) {
    if (row.detail !== false && row.notifications_ignored === true) continue;
    if (row.notification_card) {
      result.push(row);
      continue;
    }
    const failed = row.attention === 'failed' || row.attention === 'unreachable';
    if (failed && row.verdict.needs_person) result.push(row);
  }
  return result;
}

export function attentionCount(
  rows: Iterable<SessionRow>,
  launchFailures: readonly ViewerLaunchFailure[] | undefined,
): number {
  return attentionSessions(rows).length + (launchFailures?.length ?? 0);
}

/**
 * The workspace a route names, or the one to fall back to: the most recently
 * opened. Mirrors the previous viewer's selection rules.
 */
export function selectedWorkspaceId(
  route: Route,
  workspaces: readonly ViewerWorkspace[],
  rowById: (id: string) => SessionRow | undefined,
): string | undefined {
  if ('subagentParentId' in route && route.subagentParentId) {
    return rowById(route.subagentParentId)?.workspace_id;
  }
  if ('workspaceId' in route && route.workspaceId && workspaces.some(w => w.id === route.workspaceId)) {
    return route.workspaceId;
  }
  if (route.name === 'conversation') {
    const workspace = rowById(route.sessionId)?.workspace_id;
    if (workspace) return workspace;
  }
  return workspaces.reduce<ViewerWorkspace | undefined>(
    (recent, workspace) =>
      !recent || (workspace.last_opened_at || '') > (recent.last_opened_at || '') ? workspace : recent,
    undefined,
  )?.id;
}

/**
 * The route to render. A hash that names no screen opens the notifications
 * inbox when something needs attention, else the active workspace.
 */
export function effectiveRoute(
  route: Route,
  implicit: boolean,
  hasAttention: boolean,
  rememberedWorkspaceId?: string | null,
): Route {
  if (implicit && route.name === 'workspace' && !route.workspaceId) {
    if (hasAttention) return { name: 'notifications' };
    if (rememberedWorkspaceId) return { name: 'workspace', workspaceId: rememberedWorkspaceId };
  }
  return route;
}
