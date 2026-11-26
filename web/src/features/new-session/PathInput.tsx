import { useEffect, useRef, useState } from 'react';
import { completePath } from '../../api/client';
import type { CompletionKind, PathCompletion } from '../../api/types';

export interface PathInputProps {
  id: string;
  label: string;
  value: string;
  onChange: (value: string) => void;
  /** The target whose machine owns the path; omitted means the daemon's host. */
  targetId?: string;
  kind?: CompletionKind;
  placeholder?: string;
  hint?: string;
}

/**
 * A path field that asks the daemon for sibling entries while the person types.
 * Suggestions never rewrite the text: a reply is shown only while it still
 * answers the field's current value.
 */
export function PathInput({
  id,
  label,
  value,
  onChange,
  targetId,
  kind = 'directories',
  placeholder,
  hint,
}: PathInputProps) {
  const [completion, setCompletion] = useState<PathCompletion | null>(null);
  const [error, setError] = useState('');
  const timer = useRef<number | undefined>(undefined);
  const controller = useRef<AbortController | null>(null);

  useEffect(() => {
    window.clearTimeout(timer.current);
    controller.current?.abort();
    controller.current = null;
    setCompletion(null);
    setError('');
    const prefix = value.trim();
    if (!prefix) return undefined;
    timer.current = window.setTimeout(() => {
      const request = new AbortController();
      controller.current = request;
      completePath({ target_id: targetId, prefix, kind }, request.signal)
        .then(result => {
          if (!request.signal.aborted) setCompletion(result);
        })
        .catch(error => {
          if (!request.signal.aborted) setError(error instanceof Error ? error.message : String(error));
        });
    }, 250);
    return () => {
      window.clearTimeout(timer.current);
      controller.current?.abort();
    };
  }, [value, targetId, kind]);

  const candidates = completion?.candidates ?? [];

  return (
    <div className="flex min-w-0 flex-col gap-1">
      <label htmlFor={id} className="text-xs text-muted">
        {label}
      </label>
      <input
        id={id}
        value={value}
        onChange={event => onChange(event.target.value)}
        placeholder={placeholder}
        spellCheck={false}
        autoCapitalize="none"
        autoComplete="off"
        className="min-w-0 rounded-md border border-line bg-bg px-2.5 py-2 font-mono text-sm text-fg placeholder:text-muted/70"
      />
      {hint && <p className="text-xs text-muted">{hint}</p>}
      {error && <p className="text-xs text-warn">{error}</p>}
      {candidates.length > 0 && (
        <ul
          role="listbox"
          aria-label={`${label} suggestions`}
          className="max-h-44 overflow-y-auto rounded-md border border-line bg-raised"
        >
          {candidates.map(candidate => (
            <li key={candidate}>
              <button
                type="button"
                onClick={() => onChange(candidate)}
                className="w-full truncate px-2.5 py-1.5 text-left font-mono text-xs text-fg hover:bg-surface"
              >
                {candidate}
              </button>
            </li>
          ))}
        </ul>
      )}
      {completion?.truncated && <p className="text-xs text-muted">More entries exist; keep typing to narrow them.</p>}
    </div>
  );
}
