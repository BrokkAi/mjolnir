import { useEffect } from 'react';
import { useUiStore } from '../../stores/useUiStore';
import { NavContent } from './NavContent';
import { useEdgeSwipe } from './useEdgeSwipe';
import { useIsDesktop } from './useMediaQuery';

/**
 * Slide-over navigation for phones. It registers as a back layer while open,
 * so the Back button and Escape close it, and an edge swipe opens it.
 */
export function NavigationDrawer() {
  const open = useUiStore(state => state.drawerOpen);
  const setOpen = useUiStore(state => state.setDrawerOpen);
  const registerBackLayer = useUiStore(state => state.registerBackLayer);
  const desktop = useIsDesktop();

  useEdgeSwipe({
    enabled: !desktop,
    open,
    onOpen: () => setOpen(true),
    onClose: () => setOpen(false),
  });

  useEffect(() => {
    if (!open) return;
    return registerBackLayer(() => setOpen(false));
  }, [open, registerBackLayer, setOpen]);

  return (
    <div className={`fixed inset-0 z-40 md:hidden ${open ? '' : 'pointer-events-none'}`} aria-hidden={!open}>
      <div
        className={`absolute inset-0 bg-black/55 transition-opacity duration-200 ${open ? 'opacity-100' : 'opacity-0'}`}
        onClick={() => setOpen(false)}
      />
      <aside
        aria-label="Navigation"
        inert={!open}
        className={`absolute inset-y-0 left-0 w-72 max-w-[85vw] border-r border-line bg-surface pt-safe pb-safe pl-safe shadow-2xl transition-transform duration-200 ${
          open ? 'translate-x-0' : '-translate-x-full'
        }`}
      >
        <NavContent />
      </aside>
    </div>
  );
}
