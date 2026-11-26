import { NavContent } from './NavContent';

/** Persistent navigation rail at 768px and wider. */
export function DesktopSidebar() {
  return (
    <aside
      aria-label="Navigation"
      className="hidden w-60 shrink-0 border-r border-line bg-surface pl-safe md:block"
    >
      <NavContent />
    </aside>
  );
}
