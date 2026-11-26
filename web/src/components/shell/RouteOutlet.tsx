import { lazy, Suspense } from 'react';
import { LoaderCircle } from 'lucide-react';
import { useSessionStore } from '../../stores/useSessionStore';
import { effectiveRoute } from '../../stores/selectors';
import { useUiStore } from '../../stores/useUiStore';
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

  return <Suspense fallback={<Loading />}>{view}</Suspense>;
}
