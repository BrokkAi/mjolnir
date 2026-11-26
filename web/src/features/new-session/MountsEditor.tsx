import { Plus, Trash2 } from 'lucide-react';
import type { AdditionalMount, MountAccess } from '../../api/types';

const ACCESS_OPTIONS: { value: MountAccess; label: string }[] = [
  { value: 'ro', label: 'Read-only' },
  { value: 'cow', label: 'Copy-on-write' },
  { value: 'rw', label: 'Read-write' },
];

export interface MountsEditorProps {
  mounts: AdditionalMount[];
  onChange: (mounts: AdditionalMount[]) => void;
}

/** One row per attached host directory; the wire form is built by the view. */
export function MountsEditor({ mounts, onChange }: MountsEditorProps) {
  const update = (index: number, patch: Partial<AdditionalMount>) =>
    onChange(mounts.map((mount, at) => (at === index ? { ...mount, ...patch } : mount)));

  return (
    <div className="flex flex-col gap-2">
      {mounts.map((mount, index) => (
        <div
          key={index}
          className="grid grid-cols-1 gap-2 rounded-lg border border-line bg-raised p-2.5 sm:grid-cols-[1fr_1fr_auto_auto]"
        >
          <label className="flex min-w-0 flex-col gap-1 text-xs text-muted">
            <span>Host directory</span>
            <input
              aria-label={`Mount source ${index + 1}`}
              value={mount.source}
              onChange={event => update(index, { source: event.target.value })}
              placeholder="/host/path"
              spellCheck={false}
              className="min-w-0 rounded-md border border-line bg-bg px-2 py-1.5 text-sm text-fg placeholder:text-muted/70"
            />
          </label>
          <label className="flex min-w-0 flex-col gap-1 text-xs text-muted">
            <span>Destination</span>
            <input
              aria-label={`Mount destination ${index + 1}`}
              value={mount.destination}
              onChange={event => update(index, { destination: event.target.value })}
              placeholder="/workspace/path"
              spellCheck={false}
              className="min-w-0 rounded-md border border-line bg-bg px-2 py-1.5 text-sm text-fg placeholder:text-muted/70"
            />
          </label>
          <label className="flex flex-col gap-1 text-xs text-muted">
            <span>Access</span>
            <select
              aria-label={`Mount access ${index + 1}`}
              value={mount.access}
              onChange={event => update(index, { access: event.target.value as MountAccess })}
              className="rounded-md border border-line bg-bg px-2 py-1.5 text-sm text-fg"
            >
              {ACCESS_OPTIONS.map(option => (
                <option key={option.value} value={option.value}>
                  {option.label}
                </option>
              ))}
            </select>
          </label>
          <button
            type="button"
            aria-label={`Remove mount ${index + 1}`}
            onClick={() => onChange(mounts.filter((_mount, at) => at !== index))}
            className="inline-flex min-h-9 items-center justify-center self-end rounded-md border border-line px-2 text-muted hover:bg-surface hover:text-danger"
          >
            <Trash2 className="size-4" aria-hidden />
          </button>
        </div>
      ))}
      <button
        type="button"
        onClick={() => onChange([...mounts, { source: '', destination: '', access: 'ro' }])}
        className="inline-flex min-h-9 items-center gap-1.5 self-start rounded-md border border-line px-2.5 text-sm text-fg hover:bg-raised"
      >
        <Plus className="size-4" aria-hidden />
        Add mount
      </button>
      {mounts.some(mount => !mount.source.trim() || !mount.destination.trim()) && (
        <p className="text-xs text-warn">Every attached directory needs a host path and a destination.</p>
      )}
    </div>
  );
}
