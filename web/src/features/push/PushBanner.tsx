// Compact status and controls for this device's Web Push subscription.
// It is embedded in the Notifications inbox header and can be dismissed.

import { Bell, BellOff, BellRing, X } from 'lucide-react';
import { useWebPush } from './useWebPush';

export interface PushBannerProps {
  className?: string;
}

export function PushBanner({ className = '' }: PushBannerProps) {
  const push = useWebPush();
  const Icon = push.enabled ? BellRing : Bell;
  return (
    <section
      aria-label="Push notifications"
      className={`min-w-0 rounded-xl border border-line bg-raised px-3 py-2 ${className}`}
    >
      <div className="flex flex-wrap items-center gap-2">
        <span className={push.enabled ? 'text-ok' : 'text-muted'}>
          <Icon className="size-4" aria-hidden />
        </span>
        <span className="min-w-0 flex-1 basis-[12rem] break-words text-[13px] text-fg">
          {push.loading ? 'Checking notifications…' : push.statusText}
        </span>
        <div className="flex flex-wrap items-center gap-1.5">
          {push.supported && !push.enabled && (
            <button
              type="button"
              onClick={() => void push.enable()}
              disabled={push.busy || push.permission === 'denied'}
              className="min-h-9 rounded-lg bg-accent px-3 text-xs font-semibold text-accent-fg disabled:opacity-50"
            >
              Enable
            </button>
          )}
          {push.supported && push.enabled && (
            <button
              type="button"
              onClick={() => void push.disable()}
              disabled={push.busy}
              className="inline-flex min-h-9 items-center gap-1 rounded-lg border border-line bg-surface px-3 text-xs font-semibold text-fg disabled:opacity-50"
            >
              <BellOff className="size-3.5" aria-hidden />
              Disable
            </button>
          )}
          {push.supported && (
            <button
              type="button"
              onClick={() => void push.test()}
              disabled={push.busy || !push.subscribed}
              className="min-h-9 rounded-lg border border-line bg-surface px-3 text-xs font-semibold text-fg disabled:opacity-50"
            >
              Send test
            </button>
          )}
          {push.showOffer && (
            <button
              type="button"
              aria-label="Dismiss notification prompt"
              onClick={() => void push.dismissOffer()}
              className="inline-flex size-11 items-center justify-center rounded-lg text-muted hover:text-fg"
            >
              <X className="size-4" aria-hidden />
            </button>
          )}
        </div>
      </div>
      {push.showOffer && (
        <p className="mt-1 text-[12px] text-muted">
          Get notified on this device when a session needs you.
        </p>
      )}
      {push.error && (
        <p role="alert" className="mt-1 break-words text-[12px] font-medium text-danger">
          {push.error}
        </p>
      )}
    </section>
  );
}
