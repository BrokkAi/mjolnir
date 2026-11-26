import { AlertCircle, CheckCircle2, LoaderCircle } from 'lucide-react';
import { useEffect, useState, type FormEvent } from 'react';
import { ApiError, getGitIdentity, saveGitIdentity } from '../../api/client';
import type { GitIdentity } from '../../api/types';

function describe(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/**
 * The commit author sessions inherit from this host's global Git
 * configuration. A host that sets authors per repository says so instead of
 * offering a global value that would override them.
 */
export function GitIdentityCard() {
  const [identity, setIdentity] = useState<GitIdentity | null>(null);
  const [name, setName] = useState('');
  const [email, setEmail] = useState('');
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  useEffect(() => {
    const controller = new AbortController();
    let cancelled = false;
    setLoading(true);
    getGitIdentity(controller.signal)
      .then(result => {
        if (cancelled) return;
        setIdentity(result);
        setName(result.name ?? '');
        setEmail(result.email ?? '');
        setError(null);
      })
      .catch(failure => {
        if (cancelled || controller.signal.aborted) return;
        setError(describe(failure));
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
      controller.abort();
    };
  }, []);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setSaving(true);
    setError(null);
    setNotice(null);
    try {
      const saved = await saveGitIdentity(name.trim(), email.trim());
      setIdentity(saved);
      setName(saved.name ?? '');
      setEmail(saved.email ?? '');
      setNotice('Git identity saved.');
    } catch (failure) {
      // The daemon answers 422 with the sentence to show beside the fields.
      const message = describe(failure);
      setError(
        failure instanceof ApiError && failure.status === 422
          ? message
          : `Could not save the Git identity: ${message}`,
      );
    } finally {
      setSaving(false);
    }
  };

  const incomplete = identity !== null && !identity.per_directory && (!identity.name || !identity.email);

  return (
    <section
      aria-labelledby="git-identity-heading"
      className="flex flex-col gap-3 rounded-xl border border-line bg-surface p-3.5"
    >
      <div>
        <h3 id="git-identity-heading" className="text-sm font-semibold">
          Git identity
        </h3>
        <p className="text-xs text-muted">
          Sessions commit as this host&apos;s global Git author.
        </p>
      </div>

      {loading ? (
        <p role="status" className="flex items-center gap-2 text-xs text-muted">
          <LoaderCircle className="size-3.5 animate-spin" aria-hidden /> Reading the Git identity…
        </p>
      ) : identity ? (
        <>
          {identity.per_directory && (
            <p className="rounded-lg bg-raised p-2 text-xs text-muted">
              This host sets the Git author per repository directory. Sessions use each
              repository&apos;s configured identity, so a global value saved here is only a
              fallback.
            </p>
          )}
          {incomplete && (
            <p className="rounded-lg bg-warn/10 p-2 text-xs text-warn" role="status">
              Git has no complete commit author on this host, so sessions may be unable to
              commit. Set one below.
            </p>
          )}
          <form onSubmit={event => void submit(event)} className="flex flex-col gap-2.5">
            <label className="flex flex-col gap-1 text-xs">
              <span className="text-muted">Name</span>
              <input
                type="text"
                value={name}
                onChange={event => setName(event.target.value)}
                autoComplete="name"
                className="h-10 rounded-lg border border-line bg-bg px-2.5 text-sm text-fg"
              />
            </label>
            <label className="flex flex-col gap-1 text-xs">
              <span className="text-muted">Email</span>
              <input
                type="email"
                value={email}
                onChange={event => setEmail(event.target.value)}
                autoComplete="email"
                className="h-10 rounded-lg border border-line bg-bg px-2.5 text-sm text-fg"
              />
            </label>
            {error && (
              <p role="alert" className="flex items-start gap-1.5 rounded-lg bg-danger/10 p-2 text-xs text-danger">
                <AlertCircle className="mt-0.5 size-3.5 shrink-0" aria-hidden />
                {error}
              </p>
            )}
            {notice && (
              <p role="status" className="flex items-center gap-1.5 text-xs text-ok">
                <CheckCircle2 className="size-3.5" aria-hidden />
                {notice}
              </p>
            )}
            <button
              type="submit"
              disabled={saving}
              className="inline-flex h-10 items-center justify-center gap-1.5 rounded-lg bg-accent px-3 text-sm font-semibold text-accent-fg disabled:opacity-50"
            >
              {saving && <LoaderCircle className="size-4 animate-spin" aria-hidden />}
              Save Git identity
            </button>
          </form>
        </>
      ) : (
        error && (
          <p role="alert" className="rounded-lg bg-danger/10 p-2 text-xs text-danger">
            Could not read the Git identity: {error}
          </p>
        )
      )}
    </section>
  );
}
