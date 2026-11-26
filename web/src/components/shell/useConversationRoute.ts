import { useEffect } from 'react';
import { useSessionStore } from '../../stores/useSessionStore';
import { effectiveRoute } from '../../stores/selectors';
import { useTranscriptStore } from '../../stores/useTranscriptStore';
import { useUiStore } from '../../stores/useUiStore';

/**
 * The one owner of "which conversation is open". While the route names a
 * conversation, the transcript cache follows it (cached rows show at once and
 * a request goes out only if the cache is behind), and a summary or stale-detail row
 * gets its detail fetched in place. Views read; they do not open or close conversations.
 */
export function useConversationRoute(): void {
  const route = useUiStore(state => state.route);
  const implicit = useUiStore(state => state.implicitRoute);
  const sessionId = effectiveRoute(route, implicit, false).name === 'conversation' && route.name === 'conversation'
    ? route.sessionId
    : null;
  // A summary row has no detail yet; a stale detail row has detail older than the stream.
  // Either way the fetch runs while the mounted conversation, if any, stays as it is.
  const needsDetail = useSessionStore(state =>
    sessionId ? state.sessionsById[sessionId]?.detail === false || state.staleDetailIds[sessionId] === true : false,
  );

  useEffect(() => {
    if (!sessionId) return;
    void useTranscriptStore.getState().openConversation(sessionId);
    return () => useTranscriptStore.getState().closeConversation();
  }, [sessionId]);

  useEffect(() => {
    if (!sessionId || !needsDetail) return;
    useSessionStore
      .getState()
      .ensureSessionDetail(sessionId)
      .catch(error => console.warn('Could not load session detail', error));
  }, [sessionId, needsDetail]);
}
