import type { ReactNode } from 'react';
import { SessionActionModal } from '../../features/lifecycle/SessionActionModal';
import { useUiStore } from '../../stores/useUiStore';
import { BottomNav } from './BottomNav';
import { DesktopSidebar } from './DesktopSidebar';
import { MobileHeader } from './MobileHeader';
import { NavigationDrawer } from './NavigationDrawer';
import { ToastContainer } from './ToastContainer';
import { UpgradeBanner } from './UpgradeBanner';
import { useConversationRoute } from './useConversationRoute';
import { useVisualViewportInset } from './useVisualViewportInset';

/**
 * The signed-in frame. The root is exactly the dynamic viewport tall and never
 * scrolls itself; the content area below the header scrolls (or, for a
 * conversation, uses overflow-hidden so the conversation owns the single
 * transcript scroller with zero nested scrollports). Phones get a header, a
 * drawer and a bottom tab bar; 768px and wider get a sidebar instead.
 */
export function AppShell({ children }: { children: ReactNode }) {
  useVisualViewportInset();
  useConversationRoute();
  const routeName = useUiStore(state => state.route.name);
  const isConversation = routeName === 'conversation';
  return (
    <div className="flex h-dvh w-full overflow-hidden bg-bg text-fg">
      <DesktopSidebar />
      <div className="flex min-w-0 flex-1 flex-col">
        <UpgradeBanner />
        <div className="md:hidden">
          <MobileHeader />
        </div>
        <main
          className={
            isConversation
              ? 'relative flex min-h-0 flex-1 flex-col overflow-hidden pl-safe pr-safe md:pl-0 md:pr-0'
              : 'relative min-h-0 flex-1 overflow-y-auto overscroll-contain pl-safe pr-safe md:pl-0 md:pr-0'
          }
        >
          {children}
        </main>
        <BottomNav />
      </div>
      <NavigationDrawer />
      <SessionActionModal />
      <ToastContainer />
    </div>
  );
}
