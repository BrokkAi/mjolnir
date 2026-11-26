import { Component, lazy, Suspense, type ErrorInfo, type ReactNode } from 'react';
import { AlertTriangle, LoaderCircle } from 'lucide-react';
import { isChunkLoadError, reloadOnceForStaleChunk } from '../../chunkReload';
import { useSessionStore } from '../../stores/useSessionStore';
import { effectiveRoute } from '../../stores/selectors';
import { routeHash, useUiStore } from '../../stores/useUiStore';
import { useAttentionCount } from './useSessionSelectors';

// One chunk per screen. Each feature folder default-exports its view and its
// props type, so a slice replaces a file without touching this map.
const DashboardView = lazy(() => import('../../features/dashboard/DashboardView'));
const NotificationsView = lazy(() => import('../../features/notifications/NotificationsView'));
const ConversationView = lazy(() => import('../../features/conversation/ConversationView'));
const NewSessionView = lazy(() => import('../../features/new-session/NewSessionView'));
const ResumeView = lazy(() => import('../../features/resume/ResumeView'));
const MoveView = lazy(() => import('../../features/move/MoveView'));
const TargetsView = lazy(() => import('../../features/targets/TargetsView'));
const QuotaView = lazy(() => import('../../features/quota/QuotaView'));
const SettingsView = lazy(() => import('../../features/settings/SettingsView'));

function Loading() {
  return (
    <div className="grid h-full place-items-center text-muted" role="status" aria-label="Loading">
      <LoaderCircle className="size-6 animate-spin" aria-hidden />
    </div>
  );
}

interface BoundaryState {
  error: Error | null;
  reloading: boolean;
}

/**
 * Keeps a failed screen from blanking the app. A stale chunk after an upgrade
 * reloads once on its own; anything else (or a second failure) shows a card
 * with a manual reload.
 */
export class RouteErrorBoundary extends Component<{ children: ReactNode; resetKey: string }, BoundaryState> {
  state: BoundaryState = { error: null, reloading: false };

  static getDerivedStateFromError(error: unknown): Partial<BoundaryState> {
    return { error: error instanceof Error ? error : new Error(String(error)) };
  }

  componentDidUpdate(previous: { resetKey: string }) {
    // Navigating away from a failed screen starts clean; a healthy screen is never remounted.
    if (this.state.error && previous.resetKey !== this.props.resetKey) {
      this.setState({ error: null, reloading: false });
    }
  }

  componentDidCatch(error: unknown, info: ErrorInfo) {
    console.error('Route failed to render', error, info.componentStack);
    if (isChunkLoadError(error) && reloadOnceForStaleChunk()) this.setState({ reloading: true });
  }

  render() {
    const { error, reloading } = this.state;
    if (!error) return this.props.children;
    if (reloading) return <Loading />;
    return (
      <div className="grid h-full place-items-center p-6">
        <div role="alert" className="w-full max-w-sm rounded-xl border border-danger/40 bg-danger/10 p-4 text-center">
          <AlertTriangle className="mx-auto size-5 text-danger" aria-hidden />
          <p className="mt-2 text-sm text-fg">This screen could not load.</p>
          <p className="mt-1 text-xs text-muted">
            Mjolnir may have been upgraded since this page opened. Reloading loads the new version.
          </p>
          <button
            type="button"
            onClick={() => location.reload()}
            className="mt-3 inline-flex min-h-9 items-center rounded-lg border border-line bg-surface px-3 py-1.5 text-sm text-fg"
          >
            Reload Mjolnir
          </button>
        </div>
      </div>
    );
  }
}

export function RouteOutlet() {
  const route = useUiStore(state => state.route);
  const implicit = useUiStore(state => state.implicitRoute);
  const attention = useAttentionCount();
  // Until the first snapshot lands, an empty hash must not flash the wrong default.
  const loaded = useSessionStore(state => state.meta !== null);
  const shown = effectiveRoute(route, implicit, loaded && attention > 0);

  let view;
  switch (shown.name) {
    case 'notifications':
      view = <NotificationsView />;
      break;
    case 'workspace':
      view = (
        <DashboardView
          workspaceId={shown.workspaceId}
          subagentParentId={shown.subagentParentId}
          childKind={shown.childKind}
        />
      );
      break;
    case 'conversation':
      view = (
        <ConversationView
          key={shown.sessionId}
          sessionId={shown.sessionId}
          subagentParentId={shown.subagentParentId}
          childKind={shown.childKind}
          from={shown.from}
        />
      );
      break;
    case 'new':
      view = <NewSessionView workspaceId={shown.workspaceId} />;
      break;
    case 'resume':
      view = (
        <ResumeView
          workspaceId={shown.workspaceId}
          sessionId={shown.sessionId}
          wikiId={shown.wikiId}
          from={shown.from}
        />
      );
      break;
    case 'move':
      view = <MoveView workspaceId={shown.workspaceId} sessionId={shown.sessionId} />;
      break;
    case 'targets':
      view = <TargetsView />;
      break;
    case 'quota':
      view = <QuotaView />;
      break;
    case 'settings':
      view = <SettingsView path={shown.path} />;
      break;
  }

  return (
    <RouteErrorBoundary resetKey={routeHash(shown)}>
      <Suspense fallback={<Loading />}>{view}</Suspense>
    </RouteErrorBoundary>
  );
}
