import { ArrowLeft, Menu } from 'lucide-react';
import { useSessionStore, selectConnection } from '../../stores/useSessionStore';
import { effectiveRoute } from '../../stores/selectors';
import { isTopLevelRoute, useUiStore } from '../../stores/useUiStore';
import { AttentionBadge } from './NavContent';
import { parentRoute, routeTitle } from './routeMeta';
import { useAttentionCount } from './useSessionSelectors';

/** Target element for per-view header actions; see `HeaderActions`. */
export const HEADER_ACTIONS_ID = 'mj-header-actions';

const DOT: Record<string, string> = {
  live: 'bg-ok',
  connecting: 'bg-warn',
  reconnecting: 'bg-warn',
  upgrading: 'bg-warn',
  paused: 'bg-muted',
  offline: 'bg-danger',
  idle: 'bg-muted',
};

/** 52px bar plus the top safe-area inset. Shown below 768px. */
export function MobileHeader() {
  const route = useUiStore(state => state.route);
  const implicit = useUiStore(state => state.implicitRoute);
  const remembered = useUiStore(state => state.selectedWorkspaceId);
  const workspaces = useSessionStore(state => state.meta?.workspaces);
  const validRemembered = workspaces?.some(workspace => workspace.id === remembered) ? remembered : null;
  const navigate = useUiStore(state => state.navigate);
  const setDrawerOpen = useUiStore(state => state.setDrawerOpen);
  const rows = useSessionStore(state => state.sessionsById);
  const connection = useSessionStore(selectConnection);
  const attention = useAttentionCount();
  const shown = effectiveRoute(route, implicit, attention > 0, validRemembered, remembered !== null);
  const rowById = (id: string) => rows[id];
  const top = isTopLevelRoute(shown);

  return (
    <header className="shrink-0 border-b border-line bg-surface pt-safe pl-safe pr-safe">
      <div className="flex h-[52px] items-center gap-1 px-1.5">
        {top ? (
          <button
            type="button"
            aria-label="Open menu"
            onClick={() => setDrawerOpen(true)}
            className="grid size-11 place-items-center rounded-lg text-fg active:bg-raised"
          >
            <Menu className="size-5" aria-hidden />
          </button>
        ) : (
          <button
            type="button"
            aria-label="Back"
            onClick={() => navigate(parentRoute(shown, rowById))}
            className="grid size-11 place-items-center rounded-lg text-fg active:bg-raised"
          >
            <ArrowLeft className="size-5" aria-hidden />
          </button>
        )}
        <h1 className="min-w-0 flex-1 truncate text-[15px] font-semibold tracking-tight">
          {routeTitle(shown, rowById)}
        </h1>
        <div id={HEADER_ACTIONS_ID} className="flex items-center gap-1" />
        {shown.name !== 'notifications' && attention > 0 && (
          <button
            type="button"
            aria-label={`Notifications, ${attention} need attention`}
            onClick={() => navigate({ name: 'notifications' })}
            className="flex h-11 items-center px-2"
          >
            <AttentionBadge count={attention} />
          </button>
        )}
        <span
          role="img"
          aria-label={`Connection ${connection}`}
          className={`mr-2 size-2 rounded-full ${DOT[connection] ?? 'bg-muted'}`}
        />
      </div>
    </header>
  );
}
