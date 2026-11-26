import type { RawConversionPreview as RawConversionPreviewData } from '../../api/types';

export function formatBytes(bytes: number | undefined): string {
  const value = Number(bytes ?? 0);
  if (!Number.isFinite(value) || value < 0) return '0 B';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let scaled = value;
  let unit = 0;
  while (scaled >= 1024 && unit < units.length - 1) {
    scaled /= 1024;
    unit += 1;
  }
  return `${scaled < 10 && unit > 0 ? scaled.toFixed(1) : Math.round(scaled)} ${units[unit]}`;
}

/** What converting a local checkout into an isolated workspace will do. */
export function RawConversionPreview({ preview }: { preview: RawConversionPreviewData }) {
  const push = preview.push_urls.length ? preview.push_urls.join(', ') : preview.fetch_url;
  const dirty = preview.staged_files + preview.unstaged_files + preview.untracked_files;
  return (
    <div className="flex flex-col gap-2 rounded-lg border border-warn/40 bg-warn/10 p-3 text-sm">
      <p className="font-medium text-warn">This resume converts a local checkout into an isolated workspace.</p>
      <p className="text-fg">
        Clone {preview.fetch_url} (default branch {preview.default_branch}) into {preview.destination} on{' '}
        branch {preview.branch || 'a detached head'}; push to {push}.
      </p>
      <dl className="grid grid-cols-2 gap-x-3 gap-y-1 text-xs">
        <dt className="text-muted">Checkout</dt>
        <dd className="break-all font-mono text-fg">{preview.checkout}</dd>
        <dt className="text-muted">Branch</dt>
        <dd className="font-mono text-fg">{preview.branch || 'detached'}</dd>
        <dt className="text-muted">Unpushed commits</dt>
        <dd className={preview.unpushed_commits > 0 ? 'text-warn' : 'text-fg'}>{preview.unpushed_commits}</dd>
        <dt className="text-muted">Working tree</dt>
        <dd className={dirty > 0 ? 'text-warn' : 'text-fg'}>
          {preview.staged_files} staged · {preview.unstaged_files} unstaged · {preview.untracked_files}{' '}
          untracked ({formatBytes(preview.untracked_bytes)})
        </dd>
        <dt className="text-muted">Host checkout retained</dt>
        <dd className="text-fg">{preview.host_checkout_retained ? 'Yes' : 'No'}</dd>
      </dl>
      {preview.unpushed_commits > 0 && (
        <p className="text-xs text-warn">
          {preview.unpushed_commits} commit{preview.unpushed_commits === 1 ? '' : 's'} travel in the
          checkpoint. Push first when the history is large.
        </p>
      )}
      {dirty > 0 && (
        <p className="text-xs text-warn">
          Tracked and untracked changes are copied into the container. Ignored files such as build output,
          .env, and node_modules are not.
        </p>
      )}
      {preview.host_checkout_retained && (
        <p className="text-xs text-muted">
          {preview.checkout} stays on this machine but stops tracking this session. Push the branch or move
          the session back to bring changes home.
        </p>
      )}
    </div>
  );
}
