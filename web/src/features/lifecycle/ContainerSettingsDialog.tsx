import { AlertTriangle, ArrowRight, LoaderCircle, Plus, Trash2 } from 'lucide-react';
import { useState } from 'react';
import {
  normalizeMountAccess,
  serializeAdditionalMount,
  type ContainerUpgradePreview,
  type HostContainerSize,
  type MountAccess,
  type ViewerSession,
  type ViewerTarget,
} from '../../api/types';
import { useSessionStore } from '../../stores/useSessionStore';
import { useUiStore } from '../../stores/useUiStore';
import { DialogButton, ModalFrame } from './ModalFrame';

const GIB = 1024 ** 3;

function formatBytes(bytes: number): string {
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

function gigabytesText(bytes: number): string {
  const gib = bytes / GIB;
  return String(Math.round(gib * 100) / 100);
}

function initialSize(
  session: ViewerSession,
  target: ViewerTarget | undefined,
): HostContainerSize | undefined {
  const allocation = session.resource_allocation;
  if (allocation?.kind === 'container') {
    return { cpus: allocation.cpus, memory_bytes: allocation.memory_bytes };
  }
  const fallback = target?.default_resource_allocation;
  if (fallback?.kind === 'container') {
    return { cpus: fallback.cpus, memory_bytes: fallback.memory_bytes };
  }
  return target?.remembered_container_size;
}

const ACCESS_LABEL: Record<MountAccess, string> = {
  ro: 'Read only',
  cow: 'Copy on write',
  rw: 'Read/write',
};

export function ContainerSettingsDialog({
  session,
  target,
  onClose,
}: {
  session: ViewerSession;
  target: ViewerTarget | undefined;
  onClose: () => void;
}) {
  const dispatchAction = useSessionStore(state => state.dispatchAction);
  const pushToast = useUiStore(state => state.pushToast);
  const initial = initialSize(session, target);
  const [cpus, setCpus] = useState(initial ? String(initial.cpus) : '');
  const [memory, setMemory] = useState(initial ? gigabytesText(initial.memory_bytes) : '');
  const [mounts, setMounts] = useState(() =>
    (session.additional_mounts ?? []).map(normalizeMountAccess),
  );
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [preview, setPreview] = useState<ContainerUpgradePreview | null>(null);
  const [acknowledge, setAcknowledge] = useState(false);

  const limits = target?.container_host_limits;
  const isContainerTarget = target?.resource_allocation_kind === 'container';
  const upgradeInFlight = Boolean(session.operation?.cancellable);

  const parsedSize = (): { cpus: number; memoryBytes: number } | null => {
    const cpusText = cpus.trim();
    const memoryText = memory.trim();
    if (!cpusText && !memoryText) return null;
    if (!cpusText || !memoryText) {
      throw new Error('Enter both CPUs and memory, or leave both empty for the target default.');
    }
    const parsedCpus = Number(cpusText);
    const parsedMemory = Number(memoryText);
    if (!Number.isInteger(parsedCpus) || parsedCpus <= 0) {
      throw new Error('CPUs must be a positive whole number.');
    }
    if (!Number.isFinite(parsedMemory) || parsedMemory <= 0) {
      throw new Error('Memory must be a positive number of GiB.');
    }
    const memoryBytes = Math.round(parsedMemory * GIB);
    if (limits && parsedCpus > limits.cpus) {
      throw new Error(`This target allows at most ${limits.cpus} CPUs per container.`);
    }
    if (limits && memoryBytes > limits.memory_bytes) {
      throw new Error(
        `This target allows at most ${formatBytes(limits.memory_bytes)} RAM per container.`,
      );
    }
    return { cpus: parsedCpus, memoryBytes };
  };

  const updateMount = (index: number, patch: Partial<{ source: string; destination: string; access: MountAccess }>) => {
    setMounts(current =>
      current.map((mount, mountIndex) => (mountIndex === index ? { ...mount, ...patch } : mount)),
    );
  };

  const saveSettings = async () => {
    if (busy) return;
    setError(null);
    let size: { cpus: number; memoryBytes: number } | null;
    try {
      size = parsedSize();
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
      return;
    }
    const normalized = mounts.map(normalizeMountAccess);
    if (normalized.some(mount => !mount.source.trim() || !mount.destination.trim())) {
      setError('Every attached directory needs a source and a destination.');
      return;
    }
    setBusy(true);
    try {
      const result = await dispatchAction({
        action: 'set-container-settings',
        session_id: session.id,
        resource_allocation: size
          ? { kind: 'container', cpus: size.cpus, memory_bytes: size.memoryBytes }
          : null,
        mounts: normalized.map(serializeAdditionalMount),
      });
      if (result.preview) {
        setPreview(result.preview);
        setAcknowledge(false);
        setBusy(false);
        return;
      }
      pushToast('success', 'Container settings saved for the next creation.');
      onClose();
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
      setBusy(false);
    }
  };

  const upgrade = async () => {
    if (!preview || !acknowledge || busy) return;
    setBusy(true);
    setError(null);
    try {
      await dispatchAction({
        action: 'upgrade-container',
        request: {
          selection: preview,
          acknowledge_writable_layer_loss: acknowledge,
        },
      });
      pushToast('success', 'Container replacement pending.');
      onClose();
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
      setBusy(false);
    }
  };

  const cancelUpgrade = async () => {
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      await dispatchAction({ action: 'cancel-container-upgrade', session_id: session.id });
      pushToast('info', 'Container upgrade cancellation requested.');
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
    } finally {
      setBusy(false);
    }
  };

  return (
    <ModalFrame
      title="Container settings"
      subtitle={session.title}
      onClose={onClose}
      wide
      footer={
        preview ? (
          <>
            <DialogButton onClick={() => setPreview(null)} disabled={busy}>
              Back to settings
            </DialogButton>
            <DialogButton
              tone="danger"
              onClick={() => void upgrade()}
              disabled={busy || !acknowledge || !preview.available}
            >
              {busy && <LoaderCircle className="size-4 animate-spin" aria-hidden />}
              Upgrade container
            </DialogButton>
          </>
        ) : (
          <>
            <DialogButton onClick={onClose} disabled={busy}>
              Cancel
            </DialogButton>
            <DialogButton tone="primary" onClick={() => void saveSettings()} disabled={busy}>
              {busy && <LoaderCircle className="size-4 animate-spin" aria-hidden />}
              Save container settings
            </DialogButton>
          </>
        )
      }
    >
      <div className="flex flex-col gap-3.5">
        {!isContainerTarget && target && (
          <p className="rounded-lg bg-warn/10 p-2 text-xs text-warn">
            This target reports {target.resource_allocation_kind} allocation rather than container
            sizing. Changing these fields may be refused by the server.
          </p>
        )}

        {upgradeInFlight && (
          <div className="flex flex-wrap items-center justify-between gap-2 rounded-lg bg-warn/10 p-2.5 text-xs text-warn">
            <span className="inline-flex items-center gap-1.5">
              <AlertTriangle className="size-3.5" aria-hidden />
              A container upgrade is in progress.
            </span>
            <button
              type="button"
              onClick={() => void cancelUpgrade()}
              disabled={busy}
              className="h-8 rounded-lg border border-warn/40 px-2 text-xs font-semibold text-warn disabled:opacity-50"
            >
              Cancel upgrade
            </button>
          </div>
        )}

        {preview ? (
          <div className="flex flex-col gap-3">
            <div className="rounded-lg border border-line p-3">
              <div className="flex flex-wrap items-center gap-2 text-sm">
                <span className="font-mono text-xs text-muted">{preview.current_image}</span>
                <ArrowRight className="size-4 text-muted" aria-hidden />
                <span className="font-mono text-xs text-fg">{preview.target_image}</span>
              </div>
              <p className="mt-1 text-xs text-muted">
                {preview.target_image_id
                  ? `Target image ${preview.target_image_id}`
                  : 'The target image is not cached yet.'}
              </p>
            </div>
            <p className="flex items-start gap-2 rounded-lg bg-danger/10 p-3 text-sm text-danger">
              <AlertTriangle className="mt-0.5 size-4 shrink-0" aria-hidden />
              <span>{preview.warning}</span>
            </p>
            {preview.retained_mounts.length > 0 && (
              <div>
                <p className="text-xs font-semibold text-fg">Retained mounts</p>
                <ul className="ml-4 list-disc break-all text-xs text-muted">
                  {preview.retained_mounts.map(mount => (
                    <li key={mount}>{mount}</li>
                  ))}
                </ul>
              </div>
            )}
            {!preview.available && (
              <p className="rounded-lg bg-warn/10 p-2 text-xs text-warn" role="alert">
                The server reports that this replacement is not currently available.
              </p>
            )}
            <label className="flex items-start gap-2 rounded-lg border border-danger/40 p-2.5 text-xs text-fg">
              <input
                type="checkbox"
                checked={acknowledge}
                disabled={busy}
                onChange={event => setAcknowledge(event.target.checked)}
                className="mt-0.5 size-4 shrink-0"
              />
              <span>
                I understand that state in the container&apos;s writable layer, including manually
                installed packages, may be lost, and that a checkpoint does not preserve the whole
                environment.
              </span>
            </label>
          </div>
        ) : (
          <>
            <div className="grid gap-2 sm:grid-cols-2">
              <label className="flex flex-col gap-1 text-xs">
                <span className="text-muted">CPUs</span>
                <input
                  type="number"
                  min={1}
                  step={1}
                  inputMode="numeric"
                  aria-label="CPUs"
                  value={cpus}
                  onChange={event => setCpus(event.target.value)}
                  className="h-10 rounded-lg border border-line bg-bg px-2.5 text-sm text-fg"
                />
              </label>
              <label className="flex flex-col gap-1 text-xs">
                <span className="text-muted">Memory (GiB)</span>
                <input
                  type="number"
                  min={0.1}
                  step="any"
                  inputMode="decimal"
                  aria-label="Memory (GiB)"
                  value={memory}
                  onChange={event => setMemory(event.target.value)}
                  className="h-10 rounded-lg border border-line bg-bg px-2.5 text-sm text-fg"
                />
              </label>
            </div>
            <p className="text-xs text-muted">
              {limits
                ? `Host limit: ${limits.cpus} CPUs and ${formatBytes(limits.memory_bytes)} RAM per container.`
                : 'The target did not report container host limits.'}
              {target?.container_memory_hard_limit_reason
                ? ` ${target.container_memory_hard_limit_reason}`
                : ''}
            </p>

            <div className="flex flex-col gap-2">
              <div className="flex items-center justify-between gap-2">
                <p className="text-xs font-semibold uppercase tracking-wide text-muted">
                  Attached directories
                </p>
                <button
                  type="button"
                  disabled={busy}
                  onClick={() =>
                    setMounts(current => [
                      ...current,
                      { source: '', destination: '', access: 'cow' },
                    ])
                  }
                  className="inline-flex h-8 items-center gap-1 rounded-lg border border-line px-2 text-xs font-medium text-fg hover:bg-raised disabled:opacity-50"
                >
                  <Plus className="size-3.5" aria-hidden /> Add mount
                </button>
              </div>
              <p className="text-xs text-muted">
                Source directories must exist on the selected target&apos;s host.
              </p>
              {mounts.length === 0 ? (
                <p className="text-xs text-muted">No additional directories are attached.</p>
              ) : (
                <ul className="flex flex-col gap-2">
                  {mounts.map((mount, index) => (
                    <li
                      key={index}
                      className="flex flex-col gap-2 rounded-lg border border-line p-2.5"
                    >
                      <label className="flex flex-col gap-1 text-xs">
                        <span className="text-muted">Source</span>
                        <input
                          type="text"
                          aria-label={`Mount ${index + 1} source`}
                          value={mount.source}
                          onChange={event => updateMount(index, { source: event.target.value })}
                          className="h-9 rounded-lg border border-line bg-bg px-2 font-mono text-xs text-fg"
                        />
                      </label>
                      <label className="flex flex-col gap-1 text-xs">
                        <span className="text-muted">Destination</span>
                        <input
                          type="text"
                          aria-label={`Mount ${index + 1} destination`}
                          value={mount.destination}
                          onChange={event => updateMount(index, { destination: event.target.value })}
                          className="h-9 rounded-lg border border-line bg-bg px-2 font-mono text-xs text-fg"
                        />
                      </label>
                      <div className="flex items-end gap-2">
                        <label className="flex min-w-0 flex-1 flex-col gap-1 text-xs">
                          <span className="text-muted">Access</span>
                          <select
                            aria-label={`Mount ${index + 1} access`}
                            value={mount.access}
                            onChange={event =>
                              updateMount(index, { access: event.target.value as MountAccess })
                            }
                            className="h-9 rounded-lg border border-line bg-bg px-2 text-xs text-fg"
                          >
                            {(['ro', 'cow', 'rw'] as const).map(access => (
                              <option key={access} value={access}>
                                {ACCESS_LABEL[access]}
                              </option>
                            ))}
                          </select>
                        </label>
                        <button
                          type="button"
                          aria-label={`Remove mount ${index + 1}`}
                          disabled={busy}
                          onClick={() =>
                            setMounts(current => current.filter((_, mountIndex) => mountIndex !== index))
                          }
                          className="grid size-9 place-items-center rounded-lg border border-line text-muted hover:bg-raised hover:text-danger disabled:opacity-50"
                        >
                          <Trash2 className="size-3.5" aria-hidden />
                        </button>
                      </div>
                    </li>
                  ))}
                </ul>
              )}
            </div>

            <p className="text-xs text-muted">
              When the workspace can be retained, applying changed settings requires replacing the
              container. Otherwise the settings apply the next time a new environment is created.
            </p>
          </>
        )}

        {error && (
          <p role="alert" className="rounded-lg bg-danger/10 p-2 text-sm text-danger">
            {error}
          </p>
        )}
      </div>
    </ModalFrame>
  );
}
