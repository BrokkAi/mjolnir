import { LoaderCircle, WifiOff } from 'lucide-react';
import { selectConnection, useSessionStore } from '../../stores/useSessionStore';

/**
 * Says why the app is not live. An upgrade handoff is reported as such: the
 * app waits and sends the same requests again once the new daemon answers.
 */
export function UpgradeBanner() {
  const connection = useSessionStore(selectConnection);
  if (connection === 'upgrading') {
    return (
      <p role="status" className="flex shrink-0 items-center gap-2 bg-warn/15 px-4 py-2 text-sm text-warn">
        <LoaderCircle className="size-4 animate-spin" aria-hidden />
        Mjolnir is upgrading. Your requests will go through when it is back.
      </p>
    );
  }
  if (connection === 'offline') {
    return (
      <p role="status" className="flex shrink-0 items-center gap-2 bg-danger/15 px-4 py-2 text-sm text-danger">
        <WifiOff className="size-4" aria-hidden />
        Offline. Showing the last state received.
      </p>
    );
  }
  if (connection === 'reconnecting') {
    return (
      <p role="status" className="flex shrink-0 items-center gap-2 bg-warn/15 px-4 py-2 text-sm text-warn">
        <LoaderCircle className="size-4 animate-spin" aria-hidden />
        Reconnecting…
      </p>
    );
  }
  return null;
}
