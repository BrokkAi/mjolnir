import { useEffect, useState } from 'react';
import { LoaderCircle, Search } from 'lucide-react';
import { getPromptHistory } from '../../api/client';
import type { PromptHistoryScope } from '../../api/types';
import { OverlaySheet } from './OverlaySheet';
import { describeError } from './format';

/**
 * Previously submitted prompts, scoped to this session, its project or
 * everything. Picking one returns the text to the composer; it is not sent.
 */

export interface PromptHistoryModalProps {
  sessionId: string;
  onClose: () => void;
  onPick: (text: string) => void;
}

const SCOPES: { id: PromptHistoryScope; label: string }[] = [
  { id: 'session', label: 'Session' },
  { id: 'project', label: 'Project' },
  { id: 'all', label: 'All' },
];

export function PromptHistoryModal({ sessionId, onClose, onPick }: PromptHistoryModalProps) {
  const [scope, setScope] = useState<PromptHistoryScope>('session');
  const [query, setQuery] = useState('');
  const [entries, setEntries] = useState<string[]>([]);
  const [truncated, setTruncated] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const controller = new AbortController();
    const timer = setTimeout(
      () => {
        setLoading(true);
        setError(null);
        getPromptHistory(
          sessionId,
          { q: query.trim() || undefined, scope },
          controller.signal,
        )
          .then(result => {
            if (controller.signal.aborted) return;
            setEntries(result.entries);
            setTruncated(result.truncated);
          })
          .catch(failure => {
            if (controller.signal.aborted) return;
            setError(describeError(failure));
          })
          .finally(() => {
            if (!controller.signal.aborted) setLoading(false);
          });
      },
      query ? 200 : 0,
    );
    return () => {
      clearTimeout(timer);
      controller.abort();
    };
  }, [sessionId, scope, query]);

  return (
    <OverlaySheet title="Prompt history" subtitle="Pick a prompt to put back in the composer" onClose={onClose} testId="prompt-history">
      <div className="mb-3 flex gap-1 rounded-lg border border-line bg-surface p-0.5" role="tablist" aria-label="Prompt history scope">
        {SCOPES.map(item => (
          <button
            key={item.id}
            type="button"
            role="tab"
            aria-selected={scope === item.id}
            onClick={() => setScope(item.id)}
            className={`min-h-8 flex-1 rounded-md text-xs font-medium ${
              scope === item.id ? 'bg-raised text-fg' : 'text-muted hover:text-fg'
            }`}
          >
            {item.label}
          </button>
        ))}
      </div>
      <label className="mb-2 flex items-center gap-2 rounded-lg border border-line bg-surface px-2.5 py-1.5">
        <Search className="size-4 shrink-0 text-muted" aria-hidden />
        <input
          type="search"
          value={query}
          onChange={event => setQuery(event.target.value)}
          placeholder="Search prompts"
          aria-label="Search prompts"
          className="min-w-0 flex-1 bg-transparent text-[13px] text-fg outline-none placeholder:text-muted"
        />
        {loading && <LoaderCircle className="size-3.5 shrink-0 animate-spin text-muted" aria-hidden />}
      </label>
      {error && (
        <p role="alert" className="mb-2 rounded-md border border-danger/40 bg-danger/10 px-2.5 py-1.5 text-xs text-danger">
          Could not load prompt history: {error}
        </p>
      )}
      {!loading && !error && entries.length === 0 && (
        <p className="py-6 text-center text-sm text-muted">No prompts found.</p>
      )}
      <ul className="divide-y divide-line/60 overflow-hidden rounded-lg border border-line">
        {entries.map((entry, index) => (
          <li key={`${index}:${entry.slice(0, 24)}`}>
            <button
              type="button"
              onClick={() => {
                onPick(entry);
                onClose();
              }}
              className="block w-full px-2.5 py-2 text-left text-[12.5px] leading-snug text-fg hover:bg-raised/60"
            >
              <span className="line-clamp-4 whitespace-pre-wrap break-words">{entry}</span>
            </button>
          </li>
        ))}
      </ul>
      {truncated && <p className="mt-2 text-[11px] text-muted">Older prompts are not shown.</p>}
    </OverlaySheet>
  );
}
