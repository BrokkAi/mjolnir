import { LoaderCircle } from 'lucide-react';
import { useState, type FormEvent } from 'react';
import { ApiError } from '../../api/client';
import { useSessionStore } from '../../stores/useSessionStore';

function failureText(error: unknown): string {
  if (error instanceof ApiError) {
    if (error.status === 401) return 'That code is not correct. Check the six digits and try again.';
    if (error.status === 429) return error.message || 'Too many incorrect codes. Wait a moment and try again.';
    return error.message;
  }
  return error instanceof Error ? error.message : String(error);
}

/** Six-digit viewer code entry. A correct code sets the session cookie and boots the app. */
export function LoginView({ notice }: { notice?: string }) {
  const signIn = useSessionStore(state => state.signIn);
  const [code, setCode] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      await signIn(code.trim());
    } catch (failure) {
      setError(failureText(failure));
      setBusy(false);
    }
  }

  return (
    <main className="grid min-h-dvh grid-cols-[minmax(0,1fr)] place-items-center bg-bg p-4 pt-safe pb-safe text-fg">
      <form
        onSubmit={submit}
        className="flex w-full min-w-0 max-w-sm flex-col gap-4 rounded-2xl border border-line bg-surface p-6 shadow-xl"
      >
        <div className="flex items-center gap-3">
          <img src="/icon.svg" alt="" className="size-9" />
          <h1 className="text-xl font-semibold tracking-tight">Unlock Mjolnir</h1>
        </div>
        {notice && (
          <p role="status" className="rounded-lg bg-warn/15 px-3 py-2 text-sm text-warn">
            {notice}
          </p>
        )}
        <p className="text-sm text-muted">
          Enter the six-digit code shown by <code className="rounded bg-raised px-1 py-0.5">mj daemon status</code> or
          in the dashboard&rsquo;s Web viewer dialog.
        </p>
        <label className="flex flex-col gap-1.5 text-sm font-medium">
          Code
          <input
            value={code}
            onChange={event => setCode(event.target.value.replace(/\D/g, '').slice(0, 6))}
            inputMode="numeric"
            autoComplete="one-time-code"
            pattern="[0-9]{6}"
            maxLength={6}
            placeholder="000000"
            required
            autoFocus
            aria-invalid={error ? true : undefined}
            aria-describedby={error ? 'login-error' : undefined}
            className="h-12 w-full min-w-0 rounded-lg border border-line bg-bg px-3 text-center font-mono text-2xl tracking-[0.4em] text-fg placeholder:text-muted/50 focus:border-accent focus:outline-none"
          />
        </label>
        {error && (
          <p id="login-error" role="alert" className="text-sm text-danger">
            {error}
          </p>
        )}
        <button
          type="submit"
          disabled={busy || code.length !== 6}
          className="flex h-12 items-center justify-center gap-2 rounded-lg bg-accent font-semibold text-accent-fg disabled:opacity-50"
        >
          {busy && <LoaderCircle className="size-4 animate-spin" aria-hidden />}
          Enter
        </button>
      </form>
    </main>
  );
}
