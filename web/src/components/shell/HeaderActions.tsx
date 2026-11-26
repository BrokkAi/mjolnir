import { useEffect, useState, type ReactNode } from 'react';
import { createPortal } from 'react-dom';
import { HEADER_ACTIONS_ID } from './MobileHeader';

/**
 * Render a view's header buttons into the mobile header. Views use this
 * instead of owning a second header, so a conversation keeps one title bar.
 * On desktop, where the header is hidden, the actions render inline.
 */
export function HeaderActions({ children }: { children: ReactNode }) {
  const [target, setTarget] = useState<HTMLElement | null>(null);
  useEffect(() => {
    const element = document.getElementById(HEADER_ACTIONS_ID);
    // The mobile header is `display: none` on desktop: nothing to portal into.
    setTarget(element && element.offsetParent !== null ? element : null);
  }, []);
  return target ? createPortal(children, target) : <div className="flex items-center gap-1">{children}</div>;
}
