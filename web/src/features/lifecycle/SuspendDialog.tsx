import { AlertTriangle, CheckCircle2, LoaderCircle, RefreshCw } from 'lucide-react';
import { useEffect, useState, type ReactNode } from 'react';
import { suspendPreflight } from '../../api/client';
import type {
  ControllerAction,
  SuspendPreflight,
  UntrackedItem,
  ViewerSession,
} from '../../api/types';
import { useSessionStore } from '../../stores/useSessionStore';
import { useUiStore } from '../../stores/useUiStore';
import { DialogButton, ModalFrame } from './ModalFrame';

/** The exact externally tagged `UntrackedConsent::Shown` items for a preflight. */
export function shownUntrackedItems(preflight: SuspendPreflight): UntrackedItem[] {
  const items: UntrackedItem[] = [];
  for (const entry of preflight.untracked) {
    items.push({
      Path: { checkout: entry.checkout, path: entry.path, identity: entry.identity },
    });
  }
  for (const entry of preflight.nested_repositories) {
    items.push({
      NestedRepository: { checkout: entry.checkout, path: entry.path, identity: entry.identity },
    });
  }
  for (const entry of preflight.untracked_stashes) {
    items.push({
      Stash: { checkout: entry.checkout, commit: entry.commit, message: entry.message },
    });
  }
  for (const entry of preflight.keep_dir_entries) {
    items.push({
      KeepDirFile: { checkout: entry.checkout, path: entry.path, identity: entry.identity },
    });
  }
  return items;
}

export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return '—';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${unit === 0 || value >= 10 ? Math.round(value) : Math.round(value * 10) / 10} ${units[unit]}`;
}

function pathList(paths: string[], empty: string) {
  if (paths.length === 0) return <p className="text-xs text-muted">{empty}</p>;
  const shown = paths.slice(0, 20);
  return (
    <div className="flex flex-col gap-1">
      <ul className="ml-4 list-disc break-all text-xs text-fg">
        {shown.map((path, index) => (
          <li key={`${path}-${index}`}>{path}</li>
        ))}
      </ul>
      {paths.length > shown.length && (
        <p className="text-xs text-muted">and {paths.length - shown.length} more</p>
      )}
    </div>
  );
}

function Section({
  title,
  tone = 'normal',
  children,
}: {
  title: string;
  tone?: 'normal' | 'warn' | 'danger';
  children: ReactNode;
}) {
  const color = tone === 'danger' ? 'text-danger' : tone === 'warn' ? 'text-warn' : 'text-muted';
  return (
    <section className="flex flex-col gap-1.5">
      <h3 className={`text-xs font-semibold uppercase tracking-wide ${color}`}>{title}</h3>
      {children}
    </section>
  );
}

export function SuspendDialog({
  session,
  onClose,
}: {
  session: ViewerSession;
  onClose: () => void;
}) {
  const dispatchAction = useSessionStore(state => state.dispatchAction);
  const pushToast = useUiStore(state => state.pushToast);
  const [preflight, setPreflight] = useState<SuspendPreflight | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [reloadKey, setReloadKey] = useState(0);
  const [withoutCheckpoint, setWithoutCheckpoint] = useState(false);
  const [historyOnly, setHistoryOnly] = useState(false);
  const [acknowledgeUntracked, setAcknowledgeUntracked] = useState(false);
  const [acknowledgeOperations, setAcknowledgeOperations] = useState(false);

  useEffect(() => {
    const controller = new AbortController();
    let cancelled = false;
    setLoading(true);
    setError(null);
    suspendPreflight(
      session.id,
      { without_checkpoint: withoutCheckpoint, history_only: historyOnly },
      controller.signal,
    )
      .then(result => {
        if (cancelled) return;
        setPreflight(result);
        setAcknowledgeUntracked(false);
        setAcknowledgeOperations(false);
        setLoading(false);
      })
      .catch(failure => {
        if (cancelled || controller.signal.aborted) return;
        setError(failure instanceof Error ? failure.message : String(failure));
        setLoading(false);
      });
    return () => {
      cancelled = true;
      controller.abort();
    };
  }, [session.id, withoutCheckpoint, historyOnly, reloadKey]);

  const untrackedItems = preflight ? shownUntrackedItems(preflight) : [];
  const operations = preflight?.operations ?? [];
  const blockers = preflight?.blockers ?? [];
  const needsUntracked = untrackedItems.length > 0;
  const needsOperations = operations.length > 0;
  const blocked = blockers.length > 0 && !historyOnly;
  const confirmationMissing =
    (needsUntracked && !acknowledgeUntracked) || (needsOperations && !acknowledgeOperations);
  const canSubmit = Boolean(preflight) && !loading && !busy && !blocked && !confirmationMissing;

  const submit = async () => {
    if (!canSubmit) return;
    setBusy(true);
    setError(null);
    const action: Extract<ControllerAction, { action: 'suspend' }> = {
      action: 'suspend',
      session_id: session.id,
    };
    if (needsUntracked) action.discard_untracked = { Shown: untrackedItems };
    if (needsOperations) action.abort_operations = { Shown: operations };
    if (withoutCheckpoint) {
      action.without_checkpoint = true;
      action.confirm_no_checkpoint = true;
    }
    if (historyOnly) action.history_only = true;
    try {
      await dispatchAction(action);
      pushToast('success', 'Session suspension requested.');
      onClose();
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
      setBusy(false);
    }
  };

  return (
    <ModalFrame
      title="Suspend session"
      subtitle={session.title}
      onClose={onClose}
      wide
      footer={
        <>
          <DialogButton onClick={onClose} disabled={busy}>
            Cancel
          </DialogButton>
          <DialogButton tone="danger" onClick={() => void submit()} disabled={!canSubmit}>
            {busy && <LoaderCircle className="size-4 animate-spin" aria-hidden />}
            Suspend session
          </DialogButton>
        </>
      }
    >
      <div className="flex flex-col gap-3.5">
        <p className="text-sm text-muted">
          Save a recovery copy and release the environment. You can resume this session later.
        </p>

        {loading && (
          <p role="status" className="flex items-center gap-2 text-sm text-muted">
            <LoaderCircle className="size-4 animate-spin" aria-hidden /> Checking the workspace…
          </p>
        )}

        {error && (
          <div role="alert" className="flex flex-wrap items-center justify-between gap-2 rounded-lg bg-danger/10 p-2.5 text-sm text-danger">
            <span className="min-w-0 break-words">{error}</span>
            <button
              type="button"
              onClick={() => setReloadKey(key => key + 1)}
              className="inline-flex h-8 items-center gap-1 rounded-lg border border-danger/40 px-2 text-xs font-medium"
            >
              <RefreshCw className="size-3.5" aria-hidden /> Check again
            </button>
          </div>
        )}

        {preflight && (
          <>
            {preflight.raw && (
              <p className="rounded-lg bg-raised p-2 text-xs text-muted">
                Raw checkout is kept in place; untracked files and unsupported Git state do not
                travel with the snapshot.
              </p>
            )}

            {preflight.interrupting && (
              <p className="text-xs text-warn">The current turn will be interrupted.</p>
            )}

            {preflight.existing_checkpoint && (
              <p className="flex items-center gap-1.5 rounded-lg bg-ok/10 p-2 text-xs text-ok">
                <CheckCircle2 className="size-3.5" aria-hidden />
                Existing checkpoint
                {preflight.existing_checkpoint.created_at
                  ? ` from ${preflight.existing_checkpoint.created_at}`
                  : ''}
                {preflight.existing_checkpoint.age_seconds !== undefined
                  ? ` (${formatAge(preflight.existing_checkpoint.age_seconds)} old)`
                  : ''}
                {preflight.existing_checkpoint.verified ? ', verified' : ', unverified'}.
              </p>
            )}

            {blockers.length > 0 && (
              <Section title="Cannot suspend until these blockers are resolved" tone="danger">
                <ul className="flex flex-col gap-2">
                  {blockers.map((blocker, index) => (
                    <li key={`${blocker.kind}-${index}`} className="rounded-lg bg-danger/10 p-2.5">
                      <p className="text-xs font-semibold text-danger">
                        {blocker.kind.replaceAll('_', ' ')}
                      </p>
                      <div className="mt-1">{pathList(blocker.paths, 'No paths reported.')}</div>
                      <p className="mt-1 text-xs text-muted">Fix: {blocker.remedy}</p>
                    </li>
                  ))}
                </ul>
              </Section>
            )}

            {untrackedItems.length > 0 && (
              <Section
                title={`${untrackedItems.length} untracked item${untrackedItems.length === 1 ? '' : 's'} will not be kept`}
                tone="danger"
              >
                <p className="text-xs text-muted">Ignored files are never kept.</p>
                {preflight.untracked.length > 0 && (
                  <div>
                    <p className="text-xs font-medium text-fg">Files</p>
                    {pathList(
                      preflight.untracked.map(entry => entry.path),
                      'None',
                    )}
                  </div>
                )}
                {preflight.nested_repositories.length > 0 && (
                  <div>
                    <p className="text-xs font-medium text-fg">Nested repositories</p>
                    {pathList(
                      preflight.nested_repositories.map(entry => `${entry.path} (nested repository)`),
                      'None',
                    )}
                  </div>
                )}
                {preflight.untracked_stashes.length > 0 && (
                  <div>
                    <p className="text-xs font-medium text-fg">Untracked stashes</p>
                    {pathList(
                      preflight.untracked_stashes.map(
                        entry => `stash ${entry.commit.slice(0, 8)} (${entry.message})`,
                      ),
                      'None',
                    )}
                  </div>
                )}
                {preflight.keep_dir_entries.length > 0 && (
                  <div>
                    <p className="text-xs font-medium text-fg">Keep directory files</p>
                    {pathList(
                      preflight.keep_dir_entries.map(entry => `.mj/keep/${entry.path}`),
                      'None',
                    )}
                  </div>
                )}
                <label className="mt-1 flex items-start gap-2 rounded-lg bg-danger/5 p-2 text-xs text-fg">
                  <input
                    type="checkbox"
                    checked={acknowledgeUntracked}
                    disabled={busy}
                    onChange={event => setAcknowledgeUntracked(event.target.checked)}
                    className="mt-0.5 size-4 shrink-0"
                  />
                  <span>
                    I understand these untracked files, nested repositories, stashes and keep
                    files will be discarded.
                  </span>
                </label>
              </Section>
            )}

            {preflight.keep_dir_bytes > 0 && (
              <p className="text-xs text-warn">
                The keep directory holds {formatBytes(preflight.keep_dir_bytes)} that will not be
                saved on suspend.
              </p>
            )}

            {operations.length > 0 && (
              <Section title="In-progress Git operations will be aborted" tone="danger">
                <p className="text-xs text-fg">
                  {operations.map(operation => operation.kind).join(', ')}: conflict resolution is
                  discarded.
                </p>
                <label className="flex items-start gap-2 rounded-lg bg-danger/5 p-2 text-xs text-fg">
                  <input
                    type="checkbox"
                    checked={acknowledgeOperations}
                    disabled={busy}
                    onChange={event => setAcknowledgeOperations(event.target.checked)}
                    className="mt-0.5 size-4 shrink-0"
                  />
                  <span>I understand these Git operations will be aborted.</span>
                </label>
              </Section>
            )}

            {preflight.children_not_handed_back.length > 0 && (
              <Section title="Sub-agents not handed back" tone="warn">
                <p className="text-xs text-fg">
                  {preflight.children_not_handed_back.length === 1
                    ? '1 sub-agent has not handed back; suspending stops it.'
                    : `${preflight.children_not_handed_back.length} sub-agents have not handed back; suspending stops them.`}
                </p>
                {pathList(preflight.children_not_handed_back, 'None')}
              </Section>
            )}

            {preflight.owning_children_to_suspend !== undefined &&
              preflight.owning_children_to_suspend > 0 && (
                <p className="flex items-start gap-1.5 rounded-lg bg-warn/10 p-2 text-xs text-warn">
                  <AlertTriangle className="mt-0.5 size-3.5 shrink-0" aria-hidden />
                  Suspending also suspends {preflight.owning_children_to_suspend}{' '}
                  {preflight.owning_children_to_suspend === 1 ? 'child' : 'children'} with their own
                  environments and drops their untracked files.
                </p>
              )}

            {preflight.dry_run && (
              <p className="text-xs text-muted">
                Estimated transfer: {preflight.dry_run.session_state_bytes ?? 0} bytes of session
                state, {preflight.dry_run.keep_dir_bytes} bytes of keep directory
                {preflight.dry_run.git_delta_bytes !== undefined
                  ? `, ${preflight.dry_run.git_delta_bytes} bytes of Git delta`
                  : ''}
                .
              </p>
            )}

            <div className="flex flex-col gap-2 rounded-lg border border-line p-2.5">
              <p className="text-xs font-semibold text-fg">Options</p>
              <label className="flex items-start gap-2 text-xs text-fg">
                <input
                  type="checkbox"
                  checked={withoutCheckpoint}
                  disabled={busy}
                  onChange={event => {
                    setWithoutCheckpoint(event.target.checked);
                    if (event.target.checked) setHistoryOnly(false);
                  }}
                  className="mt-0.5 size-4 shrink-0"
                />
                <span>
                  Suspend without a checkpoint
                  <span className="mt-0.5 block text-muted">
                    No recovery copy is saved; only the session history can be resumed.
                  </span>
                </span>
              </label>
              <label className="flex items-start gap-2 text-xs text-fg">
                <input
                  type="checkbox"
                  checked={historyOnly}
                  disabled={busy}
                  onChange={event => {
                    setHistoryOnly(event.target.checked);
                    if (event.target.checked) setWithoutCheckpoint(false);
                  }}
                  className="mt-0.5 size-4 shrink-0"
                />
                <span>
                  Suspend with history only
                  <span className="mt-0.5 block text-muted">
                    Discard workspace Git changes and save the conversation history only. Resuming
                    starts from a fresh checkout.
                  </span>
                </span>
              </label>
            </div>
          </>
        )}
      </div>
    </ModalFrame>
  );
}

function formatAge(seconds: number): string {
  if (seconds < 60) return `${Math.max(1, Math.floor(seconds))}s`;
  if (seconds < 3_600) return `${Math.floor(seconds / 60)}m`;
  if (seconds < 86_400) return `${Math.floor(seconds / 3_600)}h`;
  return `${Math.floor(seconds / 86_400)}d`;
}
