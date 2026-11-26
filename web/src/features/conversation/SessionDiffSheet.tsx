import { useEffect, useState } from 'react';
import { FileDiff, LoaderCircle, RotateCw } from 'lucide-react';
import { getSessionDiff } from '../../api/client';
import type { SessionDiff } from '../../api/types';
import { UnifiedDiffView } from './UnifiedDiffView';
import { OverlaySheet } from './OverlaySheet';
import { describeError } from './format';

/**
 * The session's working-tree diff, filtered by file. The tracked file list is
 * the index: tapping a file asks the server for just that path, so a phone
 * never downloads a large repository diff it did not ask for.
 */

export interface SessionDiffSheetProps {
  sessionId: string;
  onClose: () => void;
}

export function SessionDiffSheet({ sessionId, onClose }: SessionDiffSheetProps) {
  const [diff, setDiff] = useState<SessionDiff | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [selectedPath, setSelectedPath] = useState<string | null>(null);
  const [reloadToken, setReloadToken] = useState(0);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setError(null);
    getSessionDiff(sessionId, selectedPath ? { paths: [selectedPath] } : {})
      .then(result => {
        if (!cancelled) setDiff(result);
      })
      .catch(failure => {
        if (!cancelled) setError(describeError(failure));
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [sessionId, selectedPath, reloadToken]);

  const tracked = diff?.tracked_files ?? [];
  const untracked = diff?.untracked_files ?? [];

  return (
    <OverlaySheet
      title="Session diff"
      subtitle={selectedPath ? `Filtered to ${selectedPath}` : 'Working tree changes'}
      onClose={onClose}
      testId="session-diff-sheet"
      footer={
        <div className="flex items-center justify-between gap-2 text-[11px] text-muted">
          <span>
            {diff ? (
              <>
                <span className="font-mono">{diff.base || 'base'}</span> →{' '}
                <span className="font-mono">{diff.head || 'working tree'}</span>
              </>
            ) : (
              'Loading…'
            )}
          </span>
          <button
            type="button"
            onClick={() => setReloadToken(value => value + 1)}
            disabled={loading}
            className="inline-flex min-h-8 items-center gap-1.5 rounded-lg border border-line px-2 py-1 text-[11px] text-fg disabled:opacity-60"
          >
            <RotateCw className={`size-3.5 ${loading ? 'animate-spin' : ''}`} aria-hidden />
            Refresh
          </button>
        </div>
      }
    >
      {loading && !diff && (
        <p className="flex items-center gap-2 py-6 text-sm text-muted" role="status">
          <LoaderCircle className="size-4 animate-spin" aria-hidden />
          Loading diff…
        </p>
      )}
      {error && (
        <div className="rounded-lg border border-danger/40 bg-danger/10 p-3">
          <p role="alert" className="text-sm text-danger">
            Could not load the diff: {error}
          </p>
          <button
            type="button"
            onClick={() => setReloadToken(value => value + 1)}
            className="mt-2 inline-flex min-h-8 items-center gap-1.5 rounded-lg border border-line bg-surface px-2 py-1 text-xs text-fg"
          >
            <RotateCw className="size-3.5" aria-hidden />
            Try again
          </button>
        </div>
      )}
      {diff && (
        <div className="space-y-4">
          {diff.head_descends_from_base === false && (
            <p className="rounded-md border border-warn/40 bg-warn/10 px-2.5 py-1.5 text-[12px] text-warn">
              The head does not descend from the base commit.
            </p>
          )}

          {tracked.length > 0 && (
            <section>
              <div className="mb-1 flex items-center justify-between gap-2">
                <h3 className="text-[11px] uppercase tracking-wide text-muted">
                  Tracked files ({tracked.length})
                </h3>
                {selectedPath && (
                  <button
                    type="button"
                    onClick={() => setSelectedPath(null)}
                    className="rounded border border-line px-2 py-0.5 text-[11px] text-muted hover:text-fg"
                  >
                    Show all files
                  </button>
                )}
              </div>
              <ul className="divide-y divide-line/60 overflow-hidden rounded-lg border border-line">
                {tracked.map(file => {
                  const selected = file.path === selectedPath;
                  return (
                    <li key={file.path}>
                      <button
                        type="button"
                        onClick={() => setSelectedPath(selected ? null : file.path)}
                        aria-pressed={selected}
                        className={`flex min-h-9 w-full items-center gap-2 px-2.5 py-1 text-left text-[12.5px] ${
                          selected ? 'bg-accent/10' : 'hover:bg-raised/60'
                        }`}
                      >
                        <FileDiff className="size-3.5 shrink-0 text-muted" aria-hidden />
                        <span className="min-w-0 flex-1 truncate font-mono">{file.path}</span>
                        {file.binary ? (
                          <span className="shrink-0 text-[10px] text-muted">binary</span>
                        ) : (
                          <span className="shrink-0 space-x-1 tabular-nums">
                            <span className="text-ok">+{file.insertions ?? 0}</span>
                            <span className="text-danger">-{file.deletions ?? 0}</span>
                          </span>
                        )}
                      </button>
                    </li>
                  );
                })}
              </ul>
            </section>
          )}

          {untracked.length > 0 && (
            <section>
              <h3 className="mb-1 text-[11px] uppercase tracking-wide text-muted">
                Untracked files{typeof diff.untracked_total === 'number' && diff.untracked_total > untracked.length
                  ? ` (${untracked.length} of ${diff.untracked_total})`
                  : ` (${untracked.length})`}
              </h3>
              <ul className="space-y-1">
                {untracked.map(path => (
                  <li
                    key={path}
                    className="truncate rounded-md border border-line bg-surface px-2 py-1 font-mono text-[11.5px] text-muted"
                  >
                    {path}
                  </li>
                ))}
              </ul>
            </section>
          )}

          <section>
            <h3 className="mb-1 text-[11px] uppercase tracking-wide text-muted">Diff</h3>
            {diff.diff.trim() ? (
              <UnifiedDiffView text={diff.diff} />
            ) : (
              <p className="rounded-md border border-line bg-surface px-3 py-2 text-[13px] text-muted">
                No tracked changes{selectedPath ? ' for this file' : ''}.
              </p>
            )}
          </section>
        </div>
      )}
    </OverlaySheet>
  );
}
