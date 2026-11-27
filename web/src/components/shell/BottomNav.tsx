import { useSessionStore } from '../../stores/useSessionStore';
import { effectiveRoute } from '../../stores/selectors';
import { isTopLevelRoute, useUiStore } from '../../stores/useUiStore';
import { AttentionBadge, NAV_ITEMS } from './NavContent';
import { useAttentionCount } from './useSessionSelectors';

/**
 * Tab bar for top-level screens on phones. It is absent inside a
 * conversation and other pushed screens, which own the full height.
 */
export function BottomNav() {
  const route = useUiStore(state => state.route);
  const implicit = useUiStore(state => state.implicitRoute);
  const navigate = useUiStore(state => state.navigate);
  const attention = useAttentionCount();
  const signedIn = useSessionStore(state => state.auth === 'signed_in');
  const shown = effectiveRoute(route, implicit, attention > 0);
  if (!signedIn || !isTopLevelRoute(shown)) return null;

  return (
    <nav
      aria-label="Primary"
      className="shrink-0 border-t border-line bg-surface pb-safe pl-safe pr-safe md:hidden"
    >
      <ul className="grid grid-cols-5">
        {NAV_ITEMS.map(item => {
          const Icon = item.icon;
          const current = shown.name === item.name;
          return (
            <li key={item.name}>
              <button
                type="button"
                aria-current={current ? 'page' : undefined}
                onClick={() => navigate(item.route)}
                className={`relative flex h-14 w-full flex-col items-center justify-center gap-0.5 text-[11px] font-medium ${
                  current ? 'font-semibold text-accent' : 'text-muted hover:text-fg'
                }`}
              >
                {current && <span className="absolute top-0 h-[3px] w-10 rounded-b-full bg-accent" />}
                <span className={`rounded-full px-3 ${current ? 'bg-accent/15' : ''}`}>
                  <Icon className="size-5" strokeWidth={current ? 2.4 : 1.75} aria-hidden />
                </span>
                {item.label}
                {item.name === 'notifications' && attention > 0 && (
                  <span className="absolute right-[calc(50%-22px)] top-1.5">
                    <AttentionBadge count={attention} />
                  </span>
                )}
              </button>
            </li>
          );
        })}
      </ul>
    </nav>
  );
}
