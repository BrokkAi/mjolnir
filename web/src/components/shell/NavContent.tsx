import { BarChart3, Bell, LayoutGrid, LogOut, Gauge, Monitor, Moon, Server, Settings, Sun } from 'lucide-react';
import type { ComponentType } from 'react';
import { useSessionStore } from '../../stores/useSessionStore';
import { useUiStore, type Route, type Theme } from '../../stores/useUiStore';
import { useAttentionCount } from './useSessionSelectors';

export interface NavItem {
  name: Route['name'];
  label: string;
  route: Route;
  icon: ComponentType<{ className?: string; 'aria-hidden'?: boolean; strokeWidth?: number }>;
}

export const NAV_ITEMS: NavItem[] = [
  { name: 'notifications', label: 'Inbox', route: { name: 'notifications' }, icon: Bell },
  { name: 'workspace', label: 'Sessions', route: { name: 'workspace' }, icon: LayoutGrid },
  { name: 'targets', label: 'Targets', route: { name: 'targets' }, icon: Server },
  { name: 'usage', label: 'Usage Dashboard', route: { name: 'usage' }, icon: BarChart3 },
  { name: 'quota', label: 'Quota', route: { name: 'quota' }, icon: Gauge },
  { name: 'settings', label: 'Settings', route: { name: 'settings', path: [] }, icon: Settings },
];

export function AttentionBadge({ count }: { count: number }) {
  if (count <= 0) return null;
  return (
    <span
      className="ml-auto min-w-5 rounded-full bg-danger px-1.5 text-center text-[11px] font-semibold leading-5 text-white"
      aria-label={`${count} need attention`}
    >
      {count > 99 ? '99+' : count}
    </span>
  );
}

const THEMES: { value: Theme; label: string; icon: NavItem['icon'] }[] = [
  { value: 'system', label: 'System', icon: Monitor },
  { value: 'dark', label: 'Dark', icon: Moon },
  { value: 'light', label: 'Light', icon: Sun },
];

function ThemePicker() {
  const theme = useUiStore(state => state.theme);
  const setTheme = useUiStore(state => state.setTheme);
  return (
    <div role="radiogroup" aria-label="Theme" className="grid grid-cols-3 gap-1 rounded-lg bg-raised p-1">
      {THEMES.map(({ value, label, icon: Icon }) => (
        <button
          key={value}
          type="button"
          role="radio"
          aria-checked={theme === value}
          onClick={() => setTheme(value)}
          className={`flex h-9 items-center justify-center gap-1.5 rounded-md text-xs font-medium ${
            theme === value ? 'bg-surface text-fg shadow-sm' : 'text-muted hover:text-fg'
          }`}
        >
          <Icon className="size-3.5" aria-hidden />
          {label}
        </button>
      ))}
    </div>
  );
}

/** The navigation list shared by the desktop sidebar and the mobile drawer. */
export function NavContent() {
  const route = useUiStore(state => state.route);
  const implicit = useUiStore(state => state.implicitRoute);
  const navigate = useUiStore(state => state.navigate);
  const signOut = useSessionStore(state => state.signOut);
  const version = useSessionStore(state => state.meta?.server_version);
  const attention = useAttentionCount();
  // An implicit route is the default screen; highlight whichever the shell shows.
  const active = implicit && route.name === 'workspace' && attention > 0 ? 'notifications' : route.name;

  return (
    <div className="flex h-full flex-col gap-4 p-3">
      <div className="flex items-center gap-2 px-2 pt-1">
        <img src="/icon.svg" alt="" className="size-7" />
        <span className="text-base font-semibold tracking-tight">Mjolnir</span>
      </div>
      <nav aria-label="Mjolnir" className="flex flex-col gap-0.5">
        {NAV_ITEMS.map(item => {
          const Icon = item.icon;
          const current = active === item.name;
          return (
            <button
              key={item.name}
              type="button"
              aria-current={current ? 'page' : undefined}
              onClick={() => navigate(item.route)}
              className={`flex h-11 items-center gap-3 rounded-lg px-3 text-sm font-medium ${
                current ? 'bg-raised text-fg' : 'text-muted hover:bg-raised/60 hover:text-fg'
              }`}
            >
              <Icon className="size-[18px]" aria-hidden />
              {item.label}
              {item.name === 'notifications' && <AttentionBadge count={attention} />}
            </button>
          );
        })}
      </nav>
      <div className="mt-auto flex flex-col gap-3">
        <ThemePicker />
        <button
          type="button"
          onClick={() => void signOut()}
          className="flex h-11 items-center gap-3 rounded-lg px-3 text-sm font-medium text-muted hover:bg-raised/60 hover:text-fg"
        >
          <LogOut className="size-[18px]" aria-hidden />
          Sign out
        </button>
        <p className="px-3 text-xs text-muted">{version ? `Mjolnir v${version}` : 'Mjolnir'}</p>
      </div>
    </div>
  );
}
