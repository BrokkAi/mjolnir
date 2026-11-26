import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { ArrowLeft, LoaderCircle, RefreshCw } from 'lucide-react';
import { preflightResume } from '../../api/client';
import {
  isSummaryRow,
  normalizeMountAccess,
  serializeAdditionalMount,
  type AdditionalMount,
  type LaunchAvailability,
  type PreflightResume,
  type ResumeQueueDisposition,
  type SessionRow,
  type ViewerProfile,
  type ViewerTarget,
} from '../../api/types';
import { useSessionStore } from '../../stores/useSessionStore';
import { useUiStore } from '../../stores/useUiStore';
import { MountsEditor } from '../new-session/MountsEditor';
import { RawConversionPreview } from './RawConversionPreview';
import { formatGiB, parseGiB } from '../gib';

export interface SuspendedSessionsProps {
  workspaceId: string;
  sessionId?: string;
}


function describe(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function availabilityOfTarget(target: ViewerTarget | undefined): {
  status: LaunchAvailability;
  reason?: string;
} {
  if (!target) return { status: 'unknown' };
  switch (target.availability.status) {
    case 'available':
      return { status: 'ready' };
    case 'missing':
      return { status: 'unavailable', reason: target.availability.reason };
    default:
      return { status: 'unknown' };
  }
}

function availabilityOfProfile(profile: ViewerProfile | undefined): {
  status: LaunchAvailability;
  reason?: string;
} {
  if (!profile) return { status: 'unknown' };
  switch (profile.availability.status) {
    case 'available':
      return { status: 'ready' };
    case 'missing':
      return { status: 'unavailable', reason: profile.availability.reason };
    default:
      return { status: 'unknown' };
  }
}

const BADGE: Record<LaunchAvailability, string> = {
  ready: 'border-ok/40 bg-ok/10 text-ok',
  stale: 'border-warn/40 bg-warn/10 text-warn',
  unavailable: 'border-danger/40 bg-danger/10 text-danger',
  unknown: 'border-line bg-raised text-muted',
};

function Badge({ status, reason }: { status: LaunchAvailability; reason?: string }) {
  return (
    <span className="flex flex-col gap-0.5">
      <span className={`w-fit rounded-full border px-2 py-0.5 text-[11px] font-medium ${BADGE[status]}`}>
        {status}
      </span>
      {reason && <span className="text-xs text-warn">{reason}</span>}
    </span>
  );
}

function recency(iso: string | undefined): string {
  if (!iso) return 'unknown time';
  const timestamp = Date.parse(iso);
  if (!Number.isFinite(timestamp)) return iso;
  return new Date(timestamp).toLocaleString([], {
    month: 'short',
    day: 'numeric',
    hour: 'numeric',
    minute: '2-digit',
  });
}

/** Suspended and resumable sessions, with the resume preflight for the chosen destination. */
export function SuspendedSessions({ workspaceId, sessionId }: SuspendedSessionsProps) {
  const sessionsById = useSessionStore(state => state.sessionsById);
  const meta = useSessionStore(state => state.meta);
  const ensureSessionDetail = useSessionStore(state => state.ensureSessionDetail);
  const dispatchAction = useSessionStore(state => state.dispatchAction);
  const pushToast = useUiStore(state => state.pushToast);
  const navigate = useUiStore(state => state.navigate);

  const [allWorkspaces, setAllWorkspaces] = useState(false);
  const [selectedId, setSelectedId] = useState(sessionId ?? '');
  const [profileId, setProfileId] = useState('');
  const [targetId, setTargetId] = useState('');
  const [queue, setQueue] = useState<ResumeQueueDisposition>('start');
  const [preflight, setPreflight] = useState<PreflightResume | null>(null);
  const [preflightLoading, setPreflightLoading] = useState(false);
  const [preflightError, setPreflightError] = useState('');
  const [conversionAcknowledged, setConversionAcknowledged] = useState(false);
  const [overrideSize, setOverrideSize] = useState(false);
  const [overrideMounts, setOverrideMounts] = useState(false);
  const [cpuText, setCpuText] = useState('');
  const [memoryText, setMemoryText] = useState('');
  const [mounts, setMounts] = useState<AdditionalMount[]>([]);
  const [actionError, setActionError] = useState('');
  const [resuming, setResuming] = useState(false);

  const initialized = useRef('');
  const preflightController = useRef<AbortController | null>(null);

  useEffect(() => {
    if (sessionId) setSelectedId(sessionId);
  }, [sessionId]);

  useEffect(() => {
    initialized.current = '';
    setPreflight(null);
    setPreflightError('');
    setConversionAcknowledged(false);
    setOverrideSize(false);
    setOverrideMounts(false);
    setActionError('');
  }, [selectedId]);

  const suspended = useMemo(() => {
    return Object.values(sessionsById)
      .filter(row => !row.subagent_parent_id)
      .filter(row => row.lifecycle === 'suspended' || row.capabilities.resume === true)
      .filter(row => allWorkspaces || !workspaceId || row.workspace_id === workspaceId)
      .sort((left, right) => (right.updated_at || '').localeCompare(left.updated_at || ''));
  }, [sessionsById, allWorkspaces, workspaceId]);

  const selected: SessionRow | undefined = selectedId ? sessionsById[selectedId] : undefined;
  const detail = selected && !isSummaryRow(selected) ? selected : undefined;
  const detailLoading = useSessionStore(state =>
    selectedId ? state.detailLoadingIds[selectedId] === true : false,
  );
  const detailError = useSessionStore(state => (selectedId ? (state.detailErrors[selectedId] ?? '') : ''));

  useEffect(() => {
    if (!selectedId) return;
    const row = useSessionStore.getState().sessionsById[selectedId];
    if (!row || row.detail !== false) return;
    void ensureSessionDetail(selectedId).catch(() => {});
  }, [selectedId, ensureSessionDetail, selected?.detail]);

  useEffect(() => {
    if (!detail || initialized.current === detail.id) return;
    initialized.current = detail.id;
    const compatible = detail.compatible_resume_targets ?? [];
    setTargetId(
      compatible.includes(detail.target_id) ? detail.target_id : (compatible[0] ?? detail.target_id),
    );
    setProfileId(
      meta?.profiles.some(profile => profile.id === detail.profile_id)
        ? detail.profile_id
        : (meta?.profiles[0]?.id ?? ''),
    );
    setQueue('start');
    setCpuText(
      detail.resource_allocation?.kind === 'container'
        ? String(detail.resource_allocation.cpus)
        : '',
    );
    setMemoryText(
      detail.resource_allocation?.kind === 'container'
        ? formatGiB(detail.resource_allocation.memory_bytes)
        : '',
    );
    setMounts((detail.additional_mounts ?? []).map(normalizeMountAccess));
    setOverrideSize(false);
    setOverrideMounts(false);
  }, [detail, meta]);

  useEffect(() => {
    if (!detail || !targetId) {
      setPreflight(null);
      return undefined;
    }
    preflightController.current?.abort();
    const controller = new AbortController();
    preflightController.current = controller;
    setPreflightLoading(true);
    setPreflightError('');
    setConversionAcknowledged(false);
    preflightResume({ session_id: detail.id, target_id: targetId }, controller.signal)
      .then(result => {
        if (!controller.signal.aborted) setPreflight(result);
      })
      .catch(error => {
        if (controller.signal.aborted) return;
        setPreflight(null);
        setPreflightError(describe(error));
      })
      .finally(() => {
        if (preflightController.current === controller) {
          preflightController.current = null;
          setPreflightLoading(false);
        }
      });
    return () => controller.abort();
  }, [detail?.id, targetId]);

  useEffect(() => () => preflightController.current?.abort(), []);

  const target = meta?.targets.find(item => item.id === targetId);
  const profile = meta?.profiles.find(item => item.id === profileId);
  const targetBadge = availabilityOfTarget(target);
  const profileBadge = availabilityOfProfile(profile);
  const queuedCount = detail
    ? (detail.queued_prompts?.length ?? 0)
    : selected && isSummaryRow(selected)
      ? selected.queued_prompt_count
      : 0;
  const incompatible = detail?.incompatible_resume_targets ?? [];

  const allocation = useMemo(() => {
    if (target?.resource_allocation_kind !== 'container' || !overrideSize) return null;
    const cpus = Number(cpuText);
    const memory = parseGiB(memoryText);
    if (!Number.isInteger(cpus) || cpus < 1 || memory === null) return null;
    return { kind: 'container' as const, cpus, memory_bytes: memory };
  }, [target, overrideSize, cpuText, memoryText]);

  const allocationError = useMemo(() => {
    if (target?.resource_allocation_kind !== 'container' || !overrideSize) return '';
    const cpus = Number(cpuText);
    const memory = parseGiB(memoryText);
    if (!Number.isInteger(cpus) || cpus < 1) return 'CPU must be a whole number of at least 1.';
    if (memory === null) return 'Memory must be a positive number of GiB.';
    const limit = target.container_capacity?.total ?? target.container_host_limits;
    if (limit && cpus > limit.cpus) return `CPU exceeds the ${limit.cpus} CPUs on this host.`;
    if (limit && memory > limit.memory_bytes) {
      return `Memory exceeds the ${formatGiB(limit.memory_bytes)} GiB on this host.`;
    }
    return '';
  }, [target, overrideSize, cpuText, memoryText]);

  const conversionBlocked = preflight?.kind === 'converting-raw-checkout' && !conversionAcknowledged;
  const canResume = Boolean(
    detail &&
      detail.capabilities.resume === true &&
      profileId &&
      targetId &&
      preflight &&
      !preflightLoading &&
      !preflightError &&
      preflight.kind !== 'unavailable' &&
      !conversionBlocked &&
      !allocationError,
  );

  const resume = useCallback(async () => {
    if (!detail || !canResume) return;
    setResuming(true);
    setActionError('');
    try {
      await dispatchAction({
        action: 'resume',
        session_id: detail.id,
        workspace_id: detail.workspace_id || workspaceId,
        profile_id: profileId,
        target_id: targetId,
        queue,
        ...(overrideSize && allocation ? { resource_allocation: allocation } : {}),
        ...(overrideMounts ? { additional_mounts: mounts.map(serializeAdditionalMount) } : {}),
      });
      pushToast('success', `Resuming ${detail.title || detail.id}.`);
      navigate({ name: 'conversation', sessionId: detail.id });
    } catch (error) {
      setActionError(describe(error));
    } finally {
      setResuming(false);
    }
  }, [
    detail,
    canResume,
    dispatchAction,
    workspaceId,
    profileId,
    targetId,
    queue,
    overrideSize,
    allocation,
    overrideMounts,
    mounts,
    pushToast,
    navigate,
  ]);

  return (
    <div className="flex flex-col gap-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <label className="flex items-center gap-2 text-sm text-fg">
          <input
            type="checkbox"
            checked={allWorkspaces}
            onChange={event => setAllWorkspaces(event.target.checked)}
          />
          All workspaces
        </label>
        <span className="text-xs text-muted">
          {suspended.length} suspended session{suspended.length === 1 ? '' : 's'}
        </span>
      </div>

      <ul className="flex flex-col gap-1.5" aria-label="Suspended sessions">
        {suspended.map(row => (
          <li key={row.id}>
            <button
              type="button"
              aria-current={row.id === selectedId}
              onClick={() => setSelectedId(row.id)}
              className={`flex w-full flex-col rounded-lg border px-3 py-2 text-left ${
                row.id === selectedId ? 'border-accent bg-accent/10' : 'border-line bg-raised hover:bg-surface'
              }`}
            >
              <span className="flex items-baseline justify-between gap-2">
                <span className="truncate text-sm font-medium text-fg">{row.title || row.id}</span>
                <span className="shrink-0 text-xs text-muted">{recency(row.updated_at)}</span>
              </span>
              <span className="truncate text-xs text-muted">
                {row.display_location || row.target_id} · {row.profile_id}
                {row.state ? ` · ${row.state}` : ''}
              </span>
            </button>
          </li>
        ))}
      </ul>
      {!suspended.length && (
        <p className="rounded-lg border border-line bg-surface p-4 text-sm text-muted">
          No suspended sessions{allWorkspaces ? '' : ' in this workspace'}.
        </p>
      )}

      {selectedId && (
        <article className="flex flex-col gap-3 rounded-xl border border-line bg-surface p-4">
          <div className="flex items-center justify-between gap-2">
            <button
              type="button"
              onClick={() => setSelectedId('')}
              className="inline-flex min-h-8 items-center gap-1.5 rounded-md border border-line px-2.5 text-xs text-fg hover:bg-raised"
            >
              <ArrowLeft className="size-3.5" aria-hidden />
              All suspended sessions
            </button>
            {detail?.capabilities.open && (
              <button
                type="button"
                onClick={() => navigate({ name: 'conversation', sessionId: selectedId })}
                className="inline-flex min-h-8 items-center rounded-md border border-line px-2.5 text-xs text-fg hover:bg-raised"
              >
                Open conversation
              </button>
            )}
          </div>

          {!selected && <p className="text-sm text-muted">This session is no longer in the snapshot.</p>}
          {selected && isSummaryRow(selected) && !detailLoading && !detailError && (
            <p className="text-sm text-muted">Loading session details…</p>
          )}
          {detailLoading && (
            <p className="flex items-center gap-2 text-sm text-muted" role="status">
              <LoaderCircle className="size-4 animate-spin" aria-hidden />
              Loading session details…
            </p>
          )}
          {detailError && (
            <div className="flex flex-col items-start gap-2" role="alert">
              <p className="text-sm text-danger">{detailError}</p>
              <button
                type="button"
                onClick={() => void ensureSessionDetail(selectedId).catch(() => {})}
                className="inline-flex min-h-8 items-center gap-1.5 rounded-md border border-line px-2.5 text-xs text-fg hover:bg-raised"
              >
                <RefreshCw className="size-3.5" aria-hidden />
                Retry
              </button>
            </div>
          )}

          {detail && (
            <>
              <div>
                <h2 className="text-base font-semibold text-fg">{detail.title || detail.id}</h2>
                <p className="mt-0.5 text-xs text-muted">
                  {detail.lifecycle} · {detail.display_location || detail.target_id} · {detail.profile_id}
                </p>
                {detail.configuration_issue && (
                  <p className="mt-1 text-sm text-danger">{detail.configuration_issue}</p>
                )}
                {detail.launch_error && <p className="mt-1 text-sm text-danger">{detail.launch_error}</p>}
              </div>

              {detail.capabilities.resume !== true && (
                <p className="rounded-lg border border-warn/40 bg-warn/10 p-3 text-sm text-warn">
                  This session cannot be resumed from the web viewer. Use the terminal to finish recovery.
                </p>
              )}

              <div className="grid gap-3 sm:grid-cols-2">
                <div className="flex flex-col gap-1">
                  <label className="flex flex-col gap-1 text-xs text-muted">
                    <span>Profile</span>
                    <select
                      value={profileId}
                      onChange={event => setProfileId(event.target.value)}
                      className="rounded-md border border-line bg-bg px-2.5 py-2 text-sm text-fg"
                    >
                      {!meta?.profiles.length && <option value="">No profiles configured</option>}
                      {meta?.profiles.map(item => (
                        <option key={item.id} value={item.id}>
                          {item.id} ({item.harness_kind})
                        </option>
                      ))}
                    </select>
                  </label>
                  <Badge status={profileBadge.status} reason={profileBadge.reason} />
                </div>
                <div className="flex flex-col gap-1">
                  <label className="flex flex-col gap-1 text-xs text-muted">
                    <span>Resume target</span>
                    <select
                      value={targetId}
                      onChange={event => setTargetId(event.target.value)}
                      className="rounded-md border border-line bg-bg px-2.5 py-2 text-sm text-fg"
                    >
                      {!detail.compatible_resume_targets?.length && <option value={detail.target_id}>{detail.target_id}</option>}
                      {detail.compatible_resume_targets?.map(id => (
                        <option key={id} value={id}>
                          {id}
                        </option>
                      ))}
                    </select>
                  </label>
                  <Badge status={targetBadge.status} reason={targetBadge.reason} />
                </div>
              </div>
              {incompatible.length > 0 && (
                <p className="text-xs text-muted">
                  Not compatible with this session: {incompatible.join(', ')}.
                </p>
              )}

              {queuedCount > 0 && (
                <fieldset className="flex flex-col gap-1.5 rounded-lg border border-line bg-raised p-3">
                  <legend className="px-1 text-xs font-medium text-fg">
                    {queuedCount} queued prompt{queuedCount === 1 ? '' : 's'}
                  </legend>
                  <label className="flex items-center gap-2 text-sm text-fg">
                    <input
                      type="radio"
                      name="resume-queue"
                      checked={queue === 'start'}
                      onChange={() => setQueue('start')}
                    />
                    Run them after resuming
                  </label>
                  <label className="flex items-center gap-2 text-sm text-fg">
                    <input
                      type="radio"
                      name="resume-queue"
                      checked={queue === 'discard'}
                      onChange={() => setQueue('discard')}
                    />
                    Discard them
                  </label>
                </fieldset>
              )}

              {preflightLoading && (
                <p className="flex items-center gap-2 text-sm text-muted" role="status">
                  <LoaderCircle className="size-4 animate-spin" aria-hidden />
                  Checking the destination…
                </p>
              )}
              {preflightError && (
                <p className="text-sm text-danger" role="alert">
                  {preflightError}
                </p>
              )}
              {preflight?.kind === 'unavailable' && (
                <div className="rounded-lg border border-danger/40 bg-danger/10 p-3 text-sm text-danger" role="alert">
                  <p className="font-medium">This session cannot resume on that target.</p>
                  <p>{preflight.detail}</p>
                </div>
              )}
              {preflight?.kind === 'converting-raw-checkout' && (
                <div className="flex flex-col gap-2">
                  <RawConversionPreview preview={preflight.preview} />
                  <label className="flex items-start gap-2 rounded-lg border border-warn/40 bg-warn/10 p-3 text-sm text-fg">
                    <input
                      type="checkbox"
                      checked={conversionAcknowledged}
                      onChange={event => setConversionAcknowledged(event.target.checked)}
                      className="mt-0.5"
                    />
                    I understand this checkout will move into an isolated workspace.
                  </label>
                </div>
              )}
              {preflight?.kind === 'ready' && (
                <p className="text-sm text-ok">This resume keeps the session's checkout where it is.</p>
              )}

              {target?.resource_allocation_kind === 'container' && (
                <details className="rounded-lg border border-line bg-raised p-3">
                  <summary className="cursor-pointer text-xs font-semibold text-fg">Resource overrides</summary>
                  <div className="mt-2 flex flex-col gap-2">
                    <label className="flex items-center gap-2 text-sm text-fg">
                      <input
                        type="checkbox"
                        checked={overrideSize}
                        onChange={event => setOverrideSize(event.target.checked)}
                      />
                      Override container size
                    </label>
                    {overrideSize && (
                      <div className="grid gap-2 sm:grid-cols-2">
                        <label className="flex flex-col gap-1 text-xs text-muted">
                          <span>CPU cores</span>
                          <input
                            type="number"
                            min={1}
                            step={1}
                            value={cpuText}
                            onChange={event => setCpuText(event.target.value)}
                            className="rounded-md border border-line bg-bg px-2.5 py-2 text-sm text-fg"
                          />
                        </label>
                        <label className="flex flex-col gap-1 text-xs text-muted">
                          <span>Memory (GiB)</span>
                          <input
                            type="number"
                            min={1}
                            step="any"
                            value={memoryText}
                            onChange={event => setMemoryText(event.target.value)}
                            className="rounded-md border border-line bg-bg px-2.5 py-2 text-sm text-fg"
                          />
                        </label>
                      </div>
                    )}
                    {allocationError && (
                      <p className="text-sm text-danger" role="alert">
                        {allocationError}
                      </p>
                    )}
                    <label className="flex items-center gap-2 text-sm text-fg">
                      <input
                        type="checkbox"
                        checked={overrideMounts}
                        onChange={event => setOverrideMounts(event.target.checked)}
                      />
                      Replace additional mounts
                    </label>
                    {overrideMounts && <MountsEditor mounts={mounts} onChange={setMounts} />}
                  </div>
                </details>
              )}

              {actionError && (
                <p className="text-sm text-danger" role="alert">
                  {actionError}
                </p>
              )}
              <button
                type="button"
                disabled={!canResume || resuming}
                onClick={() => void resume()}
                className="inline-flex min-h-10 w-full items-center justify-center gap-2 rounded-lg bg-accent px-4 text-sm font-semibold text-accent-fg disabled:opacity-50"
              >
                {resuming && <LoaderCircle className="size-4 animate-spin" aria-hidden />}
                Resume session
              </button>
              {!canResume && !resuming && preflight?.kind !== 'unavailable' && (
                <p className="text-center text-xs text-muted">
                  {preflightLoading
                    ? 'Waiting for the destination check.'
                    : conversionBlocked
                      ? 'Acknowledge the checkout conversion to resume.'
                      : 'Choose a profile and a compatible target.'}
                </p>
              )}
            </>
          )}
        </article>
      )}
    </div>
  );
}
