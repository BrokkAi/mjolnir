import { LoaderCircle } from 'lucide-react';
import { useEffect } from 'react';
import { AppShell } from './components/shell/AppShell';
import { RouteOutlet } from './components/shell/RouteOutlet';
import { LoginView } from './features/auth/LoginView';
import { useSessionStore } from './stores/useSessionStore';
import { startTranscriptSync } from './stores/useTranscriptStore';
import { installUiListeners } from './stores/useUiStore';

function BootScreen() {
  const error = useSessionStore(state => state.bootError);
  const boot = useSessionStore(state => state.boot);
  return (
    <main className="grid min-h-dvh place-items-center bg-bg p-6 pt-safe pb-safe text-fg">
      {error ? (
        <div role="alert" className="flex max-w-sm flex-col items-center gap-3 text-center">
          <p className="text-sm text-danger">Could not load Mjolnir: {error}</p>
          <button
            type="button"
            onClick={() => void boot()}
            className="h-11 rounded-lg bg-accent px-5 text-sm font-semibold text-accent-fg"
          >
            Try again
          </button>
        </div>
      ) : (
        <div role="status" aria-label="Loading" className="flex items-center gap-3 text-muted">
          <LoaderCircle className="size-5 animate-spin" aria-hidden />
          <span className="text-sm">Loading Mjolnir…</span>
        </div>
      )}
    </main>
  );
}

export function App() {
  const auth = useSessionStore(state => state.auth);

  useEffect(() => {
    const removeListeners = installUiListeners();
    const stopSync = startTranscriptSync();
    void useSessionStore.getState().boot();
    return () => {
      stopSync();
      removeListeners();
    };
  }, []);

  if (auth === 'signed_out') return <LoginView />;
  if (auth === 'checking') return <BootScreen />;
  return (
    <AppShell>
      <RouteOutlet />
    </AppShell>
  );
}
