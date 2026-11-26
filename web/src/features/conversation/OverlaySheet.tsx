import { useEffect, type ReactNode, type RefObject } from 'react';
import { X } from 'lucide-react';
import { useUiStore } from '../../stores/useUiStore';

/**
 * A modal sheet that owns the back-layer contract with the shell: opening
 * pushes a history entry, Back (or Escape through the shell) closes it, and
 * closing from the app removes the entry it pushed.
 *
 * Navigation *out of* a sheet needs care: the sheet's history entry must be
 * dropped by the `hashchange` handler instead of being popped after the new
 * route was already pushed. A sheet that navigates sets `navigatingRef` first,
 * which leaves the layer for the router to retire.
 */

export interface OverlaySheetProps {
  title: string;
  subtitle?: string;
  onClose: () => void;
  children: ReactNode;
  footer?: ReactNode;
  testId?: string;
  /** Set to `true` immediately before navigating away. */
  navigatingRef?: RefObject<boolean>;
}

export function OverlaySheet({
  title,
  subtitle,
  onClose,
  children,
  footer,
  testId,
  navigatingRef,
}: OverlaySheetProps) {
  useEffect(() => {
    const unregister = useUiStore.getState().registerBackLayer(onClose);
    return () => {
      if (navigatingRef?.current) return;
      unregister();
    };
  }, [onClose, navigatingRef]);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== 'Escape' || event.defaultPrevented) return;
      event.preventDefault();
      onClose();
    };
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, [onClose]);

  return (
    <div
      className="fixed inset-0 z-50 flex items-end justify-center bg-black/60 sm:items-center"
      role="dialog"
      aria-modal="true"
      aria-label={title}
      data-testid={testId}
    >
      <div className="absolute inset-0" aria-hidden="true" onClick={onClose} />
      <div className="relative flex max-h-[86dvh] w-full flex-col overflow-hidden rounded-t-2xl border border-line bg-bg shadow-2xl sm:max-h-[80dvh] sm:max-w-xl sm:rounded-2xl">
        <header className="flex shrink-0 items-start gap-3 border-b border-line px-4 py-3">
          <div className="min-w-0 flex-1">
            <h2 className="truncate text-[15px] font-semibold text-fg">{title}</h2>
            {subtitle && <p className="mt-0.5 truncate text-xs text-muted">{subtitle}</p>}
          </div>
          <button
            type="button"
            aria-label="Close"
            onClick={onClose}
            className="grid size-9 shrink-0 place-items-center rounded-lg text-muted hover:bg-raised hover:text-fg"
          >
            <X className="size-4" aria-hidden />
          </button>
        </header>
        <div className="min-h-0 flex-1 overflow-y-auto overscroll-contain px-4 py-3">{children}</div>
        {footer && <footer className="shrink-0 border-t border-line px-4 py-2 pb-safe">{footer}</footer>}
      </div>
    </div>
  );
}
