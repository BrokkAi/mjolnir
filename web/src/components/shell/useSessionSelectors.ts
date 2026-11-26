import { useMemo } from 'react';
import { useSessionStore } from '../../stores/useSessionStore';
import { attentionCount } from '../../stores/selectors';

/** Number of things that need the person: the badge on Notifications. */
export function useAttentionCount(): number {
  const rows = useSessionStore(state => state.sessionsById);
  const failures = useSessionStore(state => state.meta?.launch_failures);
  return useMemo(() => attentionCount(Object.values(rows), failures), [rows, failures]);
}
