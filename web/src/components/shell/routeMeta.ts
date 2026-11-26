// Titles and Back targets for routes. The shell asks here so no screen
// repeats the mapping.

import type { SessionRow } from '../../api/types';
import type { Route } from '../../stores/useUiStore';

type RowById = (id: string) => SessionRow | undefined;

export function routeTitle(route: Route, rowById: RowById): string {
  switch (route.name) {
    case 'notifications':
      return 'Notifications';
    case 'workspace':
      if (route.subagentParentId) return route.childKind === 'reviewer' ? 'Reviewers' : 'Sub-agents';
      return 'Workspaces';
    case 'conversation':
      return rowById(route.sessionId)?.title || 'Conversation';
    case 'new':
      return 'New session';
    case 'resume':
      return 'Resume';
    case 'move':
      return 'Move session';
    case 'targets':
      return 'Targets';
    case 'quota':
      return 'Quota';
    case 'settings':
      return 'Settings';
  }
}

/** Where Back goes from a screen that is not top-level. */
export function parentRoute(route: Route, rowById: RowById): Route {
  switch (route.name) {
    case 'conversation': {
      if (route.from === 'notifications') return { name: 'notifications' };
      if (route.subagentParentId) {
        return { name: 'workspace', subagentParentId: route.subagentParentId, childKind: route.childKind };
      }
      return { name: 'workspace', workspaceId: rowById(route.sessionId)?.workspace_id };
    }
    case 'new':
    case 'resume':
    case 'move':
      if (route.name === 'resume' && route.from === 'notifications') return { name: 'notifications' };
      return { name: 'workspace', workspaceId: route.workspaceId };
    case 'settings':
      return route.path.length ? { name: 'settings', path: route.path.slice(0, -1) } : { name: 'workspace' };
    case 'workspace':
      if (route.subagentParentId) {
        return { name: 'conversation', sessionId: route.subagentParentId };
      }
      return { name: 'workspace' };
    default:
      return { name: 'workspace' };
  }
}
