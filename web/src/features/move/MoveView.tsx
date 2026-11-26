import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { LoaderCircle } from 'lucide-react';
import { prepareMove } from '../../api/client';
import {
  isSummaryRow,
  normalizeMountAccess,
  serializeAdditionalMount,
  type AdditionalMount,
  type MovePreparation,
  type MoveSelection,
  type ResumeQueueDisposition,
  type RoutePreview,
  type SubagentChoice,
  type SubagentPolicy,
  type SessionResourceAllocation,
} from '../../api/types';
import { useSessionStore } from '../../stores/useSessionStore';
import { useUiStore } from '../../stores/useUiStore';
import { MountsEditor } from '../new-session/MountsEditor';
import { RawConversionPreview, formatBytes } from '../resume/RawConversionPreview';

/** Props the shell passes. Keep this shape when replacing the placeholder. */
export type MoveViewProps = { workspaceId: string; sessionId: string };

const GIB = 1024 ** 3;
const MIN_CPUS = 2;
const MIN_MEMORY = 4 * GIB;

type SubagentMode = 'native' | 'mjolnir' | 'none';

function describe(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function formatGiB(bytes: number): string {
  const whole = Math.floor(bytes / GIB);
  const remainder = bytes - whole * GIB;
  if (remainder === 0) return String(whole);
  return `${whole}${(remainder / GIB).toFixed(4).replace(/0+$/, '').replace(/\.$/, '')}`;
}

function parseGiB(text: string): number | null {
  const match = /^(\d+)(?:\.(\d*))?$/.exec(text.trim());
  if (!match) return null;
  const whole = Number(match[1]);
  const fractionText = (match[2] ?? '').slice(0, 9);
  const fraction = fractionText ? Number(fractionText) / 10 ** fractionText.length : 0;
  return Math.round((whole + fraction) * GIB);
}

function routeLabel(route: RoutePreview | string | undefined): string {
  switch (route) {
    case 'same_host':
      return 'Same host — no network transfer';
    case 'direct_or_daemon_stream':
      return 'Direct transfer, or the daemon stream when the hosts cannot reach each other';
    case 'direct_push':
      return 'Direct push to the destination';
    case 'direct_pull':
      return 'Direct pull from the source';
    case 'daemon_stream':
      return 'Relayed through the daemon stream';
    default:
      return route || 'The daemon decides the route when the move starts';
  }
}

function queueText(item: MovePreparation['queued_commands'][number]): string {
  if (item.kind && typeof item.kind === 'object' && 'set_config' in item.kind) {
    return `/${item.kind.set_config.key} ${item.kind.set_config.value}`.trim();
  }
  const parts = (item.content ?? []).flatMap(block => {
    const text = block.text;
    if (block.type === 'text' && typeof text === 'string') return [text];
    if (block.type === 'image') return ['[image attachment]'];
    if (block.type === 'resource_link') return ['[file attachment]'];
    return [];
  });
  return parts.length ? parts.join(' ') : JSON.stringify(item.content ?? item);
}

function normalizeSubagents(policy: SubagentPolicy | undefined): SubagentMode {
  return policy?.mode === 'mjolnir' ? 'mjolnir' : policy?.mode === 'none' ? 'none' : 'native';
}

export default function MoveView({ workspaceId, sessionId }: MoveViewProps) {
  const sessionsById = useSessionStore(state => state.sessionsById);
  const meta = useSessionStore(state => state.meta);
  const ensureSessionDetail = useSessionStore(state => state.ensureSessionDetail);
  const dispatchAction = useSessionStore(state => state.dispatchAction);
  const pushToast = useUiStore(state => state.pushToast);
  const navigate = useUiStore(state => state.navigate);

  const row = sessionsById[sessionId];
  const detail = row && !isSummaryRow(row) ? row : undefined;
  const loading = useSessionStore(state => state.detailLoadingIds[sessionId] === true);
  const detailError = useSessionStore(state => state.detailErrors[sessionId] ?? '');
  const [localDetailError, setLocalDetailError] = useState('');

  const [profileId, setProfileId] = useState('');
  const [targetId, setTargetId] = useState('');
  const [clearAllocation, setClearAllocation] = useState(false);
  const [cpuText, setCpuText] = useState('');
  const [memoryText, setMemoryText] = useState('');
  const [mounts, setMounts] = useState<AdditionalMount[]>([]);
  const [subagentMode, setSubagentMode] = useState<SubagentMode>('native');
  const [oracleEnabled, setOracleEnabled] = useState(false);
  const [oracleModel, setOracleModel] = useState('');
  const [oracleEffort, setOracleEffort] = useState('');

  const [preparation, setPreparation] = useState<MovePreparation | null>(null);
  const [preparing, setPreparing] = useState(false);
  const [prepareError, setPrepareError] = useState('');
  const [skipKeepPaths, setSkipKeepPaths] = useState<string[]>([]);
  const [skipInput, setSkipInput] = useState('');
  const [queueChoice, setQueueChoice] = useState<ResumeQueueDisposition>('discard');
  const [acknowledge, setAcknowledge] = useState(false);
  const [moving, setMoving] = useState(false);
  const [moveError, setMoveError] = useState('');

  const initialized = useRef('');
  const allocationTarget = useRef('');
  const prepareController = useRef<AbortController | null>(null);

  useEffect(() => {
    void ensureSessionDetail(sessionId).catch(error => setLocalDetailError(describe(error)));
  }, [sessionId, ensureSessionDetail]);

  useEffect(() => {
    initialized.current = '';
    allocationTarget.current = '';
    setPreparation(null);
    setPrepareError('');
    setMoveError('');
    setAcknowledge(false);
    setSkipKeepPaths([]);
    setLocalDetailError('');
  }, [sessionId]);

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
    setMounts((detail.additional_mounts ?? []).map(normalizeMountAccess));
    setSubagentMode(normalizeSubagents(detail.subagents));
    setOracleEnabled(detail.oracle !== false);
    setOracleModel(detail.oracle_choice?.model ?? '');
    setOracleEffort(detail.oracle_choice?.effort ?? '');
    setQueueChoice('discard');
  }, [detail, meta]);

  const destinationTarget = useMemo(
    () => meta?.targets.find(target => target.id === targetId),
    [meta, targetId],
  );
  const destinationKind = destinationTarget?.resource_allocation_kind ?? 'fixed';

  useEffect(() => {
    if (!targetId || !meta || allocationTarget.current === targetId) return;
    allocationTarget.current = targetId;
    const target = meta.targets.find(item => item.id === targetId);
    if (target?.resource_allocation_kind !== 'container') {
      setClearAllocation(true);
      setCpuText('');
      setMemoryText('');
      return;
    }
    setClearAllocation(false);
    const current = detail?.resource_allocation?.kind === 'container' ? detail.resource_allocation : null;
    const fallback = target.default_resource_allocation;
    const remembered = target.remembered_container_size;
    const cpus =
      current?.cpus ?? remembered?.cpus ?? (fallback?.kind === 'container' ? fallback.cpus : MIN_CPUS);
    const memory =
      current?.memory_bytes ??
      remembered?.memory_bytes ??
      (fallback?.kind === 'container' ? fallback.memory_bytes : MIN_MEMORY);
    setCpuText(String(cpus));
    setMemoryText(formatGiB(memory));
  }, [targetId, meta, detail]);

  useEffect(() => () => prepareController.current?.abort(), []);

  const allocation = useMemo<SessionResourceAllocation | null>(() => {
    if (destinationKind !== 'container' || clearAllocation) return null;
    const cpus = Number(cpuText);
    const memory = parseGiB(memoryText);
    if (!Number.isInteger(cpus) || cpus < 1 || memory === null) return null;
    return { kind: 'container', cpus, memory_bytes: memory };
  }, [destinationKind, clearAllocation, cpuText, memoryText]);

  const allocationError = useMemo(() => {
    if (destinationKind !== 'container' || clearAllocation) return '';
    const cpus = Number(cpuText);
    if (!Number.isInteger(cpus) || cpus < MIN_CPUS) return `CPU must be a whole number of at least ${MIN_CPUS}.`;
    const memory = parseGiB(memoryText);
    if (memory === null) return 'Memory must be a positive number of GiB.';
    if (memory < MIN_MEMORY) return 'Memory must be at least 4 GiB.';
    const limit = destinationTarget?.container_capacity?.total ?? destinationTarget?.container_host_limits;
    if (limit && cpus > limit.cpus) return `CPU exceeds the ${limit.cpus} CPUs on this host.`;
    if (limit && memory > limit.memory_bytes) {
      return `Memory exceeds the ${formatGiB(limit.memory_bytes)} GiB on this host.`;
    }
    return '';
  }, [destinationKind, clearAllocation, cpuText, memoryText, destinationTarget]);

  const mountsIncomplete = mounts.some(mount => !mount.source.trim() || !mount.destination.trim());

  const invalidate = useCallback(() => {
    prepareController.current?.abort();
    setPreparation(null);
    setPrepareError('');
    setSkipKeepPaths([]);
    setAcknowledge(false);
  }, []);

  const buildSelection = useCallback(
    (skip: string[]): MoveSelection => {
      let subagents: SubagentPolicy;
      if (subagentMode === 'none') subagents = { mode: 'none' };
      else if (subagentMode === 'mjolnir') {
        const stored = detail?.subagents;
        subagents =
          stored?.mode === 'mjolnir'
            ? {
                mode: 'mjolnir',
                ...(stored.straightforward ? { straightforward: stored.straightforward } : {}),
                ...(stored.subtle ? { subtle: stored.subtle } : {}),
                ...(stored.orchestrators ? { orchestrators: true } : {}),
              }
            : { mode: 'mjolnir' };
      } else {
        subagents = { mode: 'native' };
      }
      const choice: SubagentChoice | null = oracleModel
        ? { model: oracleModel, ...(oracleEffort ? { effort: oracleEffort } : {}) }
        : null;
      return {
        session_id: sessionId,
        profile_id: profileId || null,
        target_template_id: targetId || null,
        clear_resource_allocation: destinationKind !== 'container' || clearAllocation,
        additional_mounts: mounts.length ? mounts.map(serializeAdditionalMount) : null,
        resource_allocation: allocation,
        subagents,
        oracle: oracleEnabled,
        oracle_choice: oracleEnabled ? choice : null,
        skip_keep_paths: skip,
      };
    },
    [
      sessionId,
      profileId,
      targetId,
      destinationKind,
      clearAllocation,
      mounts,
      allocation,
      subagentMode,
      detail,
      oracleEnabled,
      oracleModel,
      oracleEffort,
    ],
  );

  const runPrepare = useCallback(
    async (skip: string[]) => {
      if (!detail) return;
      if (!targetId || !profileId) {
        setPrepareError('Choose a destination profile and target.');
        return;
      }
      if (allocationError) {
        setPrepareError(allocationError);
        return;
      }
      if (mountsIncomplete) {
        setPrepareError('Every attached directory needs a host path and a destination.');
        return;
      }
      prepareController.current?.abort();
      const controller = new AbortController();
      prepareController.current = controller;
      setPreparing(true);
      setPrepareError('');
      try {
        const next = await prepareMove(buildSelection(skip), controller.signal);
        if (controller.signal.aborted) return;
        setPreparation(next);
        setSkipKeepPaths(next.selection.skip_keep_paths ?? skip);
        setAcknowledge(false);
      } catch (error) {
        if (!controller.signal.aborted) setPrepareError(describe(error));
      } finally {
        if (prepareController.current === controller) {
          prepareController.current = null;
          setPreparing(false);
        }
      }
    },
    [detail, targetId, profileId, allocationError, mountsIncomplete, buildSelection],
  );

  const toggleSkip = useCallback(
    (path: string, skipped: boolean) => {
      const next = skipped
        ? [...new Set([...skipKeepPaths, path])]
        : skipKeepPaths.filter(item => item !== path);
      setSkipKeepPaths(next);
      void runPrepare(next);
    },
    [skipKeepPaths, runPrepare],
  );

  const addSkip = useCallback(() => {
    const path = skipInput.trim();
    if (!path) return;
    const next = [...new Set([...skipKeepPaths, path])];
    setSkipInput('');
    setSkipKeepPaths(next);
    void runPrepare(next);
  }, [skipInput, skipKeepPaths, runPrepare]);

  const confirm = useCallback(async () => {
    if (!preparation || moving) return;
    if (preparation.active && !acknowledge) return;
    setMoving(true);
    setMoveError('');
    const queue: ResumeQueueDisposition | null =
      preparation.locked_queue ??
      (preparation.queued_commands.length ? queueChoice : null);
    try {
      await dispatchAction({
        action: 'move',
        request: {
          preparation,
          queue,
          acknowledge_interruption: preparation.active ? acknowledge : false,
        },
      });
      pushToast('success', `Moving ${detail?.title || sessionId}.`);
      navigate({ name: 'conversation', sessionId });
    } catch (error) {
      setMoveError(describe(error));
    } finally {
      setMoving(false);
    }
  }, [
    preparation,
    moving,
    acknowledge,
    queueChoice,
    dispatchAction,
    pushToast,
    detail,
    sessionId,
    navigate,
  ]);

  const canPrepare =
    Boolean(detail) &&
    Boolean(targetId) &&
    Boolean(profileId) &&
    !preparing &&
    !allocationError &&
    !mountsIncomplete;
  const canConfirm = Boolean(preparation) && !preparing && !moving && (!preparation?.active || acknowledge);

  const capabilityChoices = meta?.profile_capabilities?.profiles?.[profileId]?.choices;
  const oracleModels = capabilityChoices?.status === 'ready' ? capabilityChoices.value.models : [];
  const oracleEfforts = capabilityChoices?.status === 'ready' ? capabilityChoices.value.efforts : [];
  const compatibleTargets = useMemo(() => {
    const ids = [
      ...(detail?.compatible_resume_targets ?? []),
      ...(detail?.target_id ? [detail.target_id] : []),
    ];
    return [...new Set(ids)];
  }, [detail]);
  const sourceAllocation = detail?.resource_allocation;
  const suspend = preparation?.suspend_preflight;

  return (
    <section className="mx-auto flex w-full max-w-3xl flex-col gap-3 p-4">
      <div>
        <h1 className="text-lg font-semibold tracking-tight text-fg">Move session</h1>
        <p className="mt-0.5 text-xs text-muted">
          Preparing is read-only. Nothing is stopped or provisioned until you confirm.
        </p>
      </div>

      {(loading || (!detail && !detailError && !localDetailError)) && (
        <p className="flex items-center gap-2 text-sm text-muted" role="status">
          <LoaderCircle className="size-4 animate-spin" aria-hidden />
          Loading session details…
        </p>
      )}
      {(detailError || localDetailError) && (
        <p className="text-sm text-danger" role="alert">
          {detailError || localDetailError}
        </p>
      )}

      {detail && (
        <>
          <article className="flex flex-col gap-1 rounded-xl border border-line bg-surface p-4">
            <h2 className="text-base font-semibold text-fg">{detail.title || detail.id}</h2>
            <p className="text-xs text-muted">
              {detail.profile_id} · {detail.target_id} · {detail.display_location || detail.lifecycle}
            </p>
            <p className="text-xs text-muted">
              {sourceAllocation?.kind === 'container'
                ? `Container: ${sourceAllocation.cpus} CPUs · ${formatGiB(sourceAllocation.memory_bytes)} GiB RAM`
                : sourceAllocation?.kind === 'aws-ec2'
                  ? `EC2: ${sourceAllocation.instance_type} (${sourceAllocation.vcpus} vCPUs · ${formatGiB(sourceAllocation.memory_bytes)} GiB RAM)`
                  : 'No container sizing recorded.'}
              {(detail.additional_mounts?.length ?? 0) > 0
                ? ` · ${detail.additional_mounts?.length} attached director${detail.additional_mounts?.length === 1 ? 'y' : 'ies'}`
                : ''}
            </p>
            {detail.move_recovery && (
              <p className="text-xs text-warn">
                Previous move {detail.move_recovery.phase}
                {detail.move_recovery.failure_notice ? `: ${detail.move_recovery.failure_notice}` : '.'}
              </p>
            )}
          </article>

          <div className="flex flex-col gap-3 rounded-xl border border-line bg-surface p-4">
            <h2 className="text-sm font-semibold text-fg">Destination</h2>
            <div className="grid gap-3 sm:grid-cols-2">
              <label className="flex flex-col gap-1 text-xs text-muted">
                <span>Profile</span>
                <select
                  value={profileId}
                  onChange={event => {
                    setProfileId(event.target.value);
                    setOracleModel('');
                    setOracleEffort('');
                    invalidate();
                  }}
                  className="rounded-md border border-line bg-bg px-2.5 py-2 text-sm text-fg"
                >
                  {!meta?.profiles.length && <option value="">No profiles configured</option>}
                  {meta?.profiles.map(profile => (
                    <option key={profile.id} value={profile.id}>
                      {profile.id} ({profile.harness_kind})
                    </option>
                  ))}
                </select>
              </label>
              <label className="flex flex-col gap-1 text-xs text-muted">
                <span>Target</span>
                <select
                  value={targetId}
                  onChange={event => {
                    setTargetId(event.target.value);
                    invalidate();
                  }}
                  className="rounded-md border border-line bg-bg px-2.5 py-2 text-sm text-fg"
                >
                  {!compatibleTargets.length && <option value="">No compatible targets</option>}
                  {compatibleTargets.map(id => (
                    <option key={id} value={id}>
                      {id}
                      {id === detail.target_id ? ' (current)' : ''}
                    </option>
                  ))}
                </select>
              </label>
            </div>

            {destinationKind === 'container' ? (
              <div className="flex flex-col gap-2 border-t border-line pt-3">
                <label className="flex items-center gap-2 text-sm text-fg">
                  <input
                    type="checkbox"
                    checked={clearAllocation}
                    onChange={event => {
                      setClearAllocation(event.target.checked);
                      invalidate();
                    }}
                  />
                  Use the destination&apos;s default resource sizing
                </label>
                {!clearAllocation && (
                  <>
                    <div className="grid gap-2 sm:grid-cols-2">
                      <label className="flex flex-col gap-1 text-xs text-muted">
                        <span>CPU cores</span>
                        <input
                          type="number"
                          min={MIN_CPUS}
                          step={1}
                          value={cpuText}
                          onChange={event => {
                            setCpuText(event.target.value);
                            invalidate();
                          }}
                          className="rounded-md border border-line bg-bg px-2.5 py-2 text-sm text-fg"
                        />
                      </label>
                      <label className="flex flex-col gap-1 text-xs text-muted">
                        <span>Memory (GiB)</span>
                        <input
                          type="number"
                          min={4}
                          step="any"
                          value={memoryText}
                          onChange={event => {
                            setMemoryText(event.target.value);
                            invalidate();
                          }}
                          className="rounded-md border border-line bg-bg px-2.5 py-2 text-sm text-fg"
                        />
                      </label>
                    </div>
                    {destinationTarget?.container_capacity?.complete && (
                      <p className="text-xs text-muted">
                        Host: {destinationTarget.container_capacity.total.cpus} CPUs and{' '}
                        {formatGiB(destinationTarget.container_capacity.total.memory_bytes)} GiB RAM · left:{' '}
                        {destinationTarget.container_capacity.remaining.cpus} CPUs and{' '}
                        {formatGiB(destinationTarget.container_capacity.remaining.memory_bytes)} GiB RAM.
                      </p>
                    )}
                    {allocationError && (
                      <p className="text-sm text-danger" role="alert">
                        {allocationError}
                      </p>
                    )}
                  </>
                )}
              </div>
            ) : (
              <p className="text-xs text-muted">
                This target does not use container sizing; the session&apos;s allocation is cleared.
              </p>
            )}

            <details className="border-t border-line pt-3" open>
              <summary className="cursor-pointer text-xs font-semibold text-fg">Attached directories</summary>
              <div className="mt-2">
                <MountsEditor
                  mounts={mounts}
                  onChange={next => {
                    setMounts(next);
                    invalidate();
                  }}
                />
              </div>
            </details>

            <div className="grid gap-2 border-t border-line pt-3 sm:grid-cols-2">
              <label className="flex flex-col gap-1 text-xs text-muted">
                <span>Sub-agent policy</span>
                <select
                  value={subagentMode}
                  onChange={event => {
                    setSubagentMode(event.target.value as SubagentMode);
                    invalidate();
                  }}
                  className="rounded-md border border-line bg-bg px-2.5 py-2 text-sm text-fg"
                >
                  <option value="native">Native</option>
                  <option value="mjolnir">Mjolnir</option>
                  <option value="none">None</option>
                </select>
              </label>
              <div className="flex flex-col gap-1">
                <label className="flex items-center gap-2 text-sm text-fg">
                  <input
                    type="checkbox"
                    checked={oracleEnabled}
                    onChange={event => {
                      setOracleEnabled(event.target.checked);
                      invalidate();
                    }}
                  />
                  Enable the oracle tool
                </label>
                {oracleEnabled && (
                  <div className="grid gap-2 sm:grid-cols-2">
                    <label className="flex flex-col gap-1 text-xs text-muted">
                      <span>Oracle model</span>
                      <select
                        value={oracleModel}
                        onChange={event => {
                          setOracleModel(event.target.value);
                          invalidate();
                        }}
                        className="rounded-md border border-line bg-bg px-2.5 py-2 text-sm text-fg"
                      >
                        <option value="">Profile default</option>
                        {oracleModels.map(choice => (
                          <option key={choice.value} value={choice.value}>
                            {choice.name || choice.value}
                          </option>
                        ))}
                      </select>
                    </label>
                    <label className="flex flex-col gap-1 text-xs text-muted">
                      <span>Oracle effort</span>
                      <select
                        value={oracleEffort}
                        onChange={event => {
                          setOracleEffort(event.target.value);
                          invalidate();
                        }}
                        className="rounded-md border border-line bg-bg px-2.5 py-2 text-sm text-fg"
                      >
                        <option value="">Profile default</option>
                        {oracleEfforts.map(choice => (
                          <option key={choice.value} value={choice.value}>
                            {choice.name || choice.value}
                          </option>
                        ))}
                      </select>
                    </label>
                  </div>
                )}
              </div>
            </div>

            {prepareError && (
              <p className="text-sm text-danger" role="alert">
                {prepareError}
              </p>
            )}
            <button
              type="button"
              disabled={!canPrepare}
              onClick={() => void runPrepare(skipKeepPaths)}
              className="inline-flex min-h-10 w-full items-center justify-center gap-2 rounded-lg bg-accent px-4 text-sm font-semibold text-accent-fg disabled:opacity-50"
            >
              {preparing && <LoaderCircle className="size-4 animate-spin" aria-hidden />}
              Prepare move
            </button>
          </div>

          {preparation && (
            <div className="flex flex-col gap-3 rounded-xl border border-line bg-surface p-4">
              <h2 className="text-sm font-semibold text-fg">Preparation</h2>
              <p className="text-sm text-fg">
                From {preparation.source_profile_id} / {preparation.source_target_template_id} to{' '}
                {preparation.selection.profile_id || detail.profile_id} /{' '}
                {preparation.selection.target_template_id || detail.target_id}.
              </p>
              <dl className="grid grid-cols-2 gap-x-3 gap-y-1 text-xs">
                <dt className="text-muted">Destination checks</dt>
                <dd className="text-fg">
                  {preparation.destination_checks === 'after_provisioning'
                    ? 'After the destination is provisioned'
                    : 'Checked before the move'}
                </dd>
                <dt className="text-muted">Transfer route</dt>
                <dd className="text-fg">
                  {routeLabel(suspend?.dry_run?.transfer_route ?? preparation.route_preview)}
                </dd>
                <dt className="text-muted">Resource sizing</dt>
                <dd className="text-fg">
                  {preparation.selection.clear_resource_allocation
                    ? 'Destination defaults'
                    : 'Source sizing retained'}
                </dd>
              </dl>

              {preparation.in_place && (
                <p className="rounded-lg border border-line bg-raised p-3 text-sm text-fg">
                  In place: only the harness and profile are replaced. The environment and workspace stay.
                </p>
              )}
              {preparation.cross_harness && (
                <p className="rounded-lg border border-warn/40 bg-warn/10 p-3 text-sm text-warn">
                  The destination uses a different harness or model service. Harness-private state is
                  rebuilt from the canonical transcript.
                </p>
              )}
              {preparation.stops_subagents && (
                <p className="rounded-lg border border-warn/40 bg-warn/10 p-3 text-sm text-warn">
                  Live sub-agents are stopped before the source closes. The session is told which when it
                  resumes.
                </p>
              )}
              {preparation.source_unavailable && (
                <p className="rounded-lg border border-warn/40 bg-warn/10 p-3 text-sm text-warn">
                  The source is unavailable. The move recovers its saved data without starting the old
                  harness.
                </p>
              )}
              {preparation.sized_by_destination_default && (
                <p className="text-xs text-muted">
                  The destination's default sizing was applied because no size was chosen.
                </p>
              )}

              {preparation.container_upgrade && (
                <div className="flex flex-col gap-1 rounded-lg border border-warn/40 bg-warn/10 p-3 text-sm">
                  <p className="font-medium text-warn">Container image replacement</p>
                  <p className="text-fg">
                    {preparation.container_upgrade.current_image} →{' '}
                    {preparation.container_upgrade.target_image || 'destination default'}
                  </p>
                  <p className={preparation.container_upgrade.available ? 'text-muted' : 'text-danger'}>
                    {preparation.container_upgrade.warning}
                  </p>
                  {preparation.container_upgrade.retained_mounts.length > 0 && (
                    <p className="text-xs text-muted">
                      Retained mounts: {preparation.container_upgrade.retained_mounts.join(', ')}
                    </p>
                  )}
                </div>
              )}

              {preparation.conversion && <RawConversionPreview preview={preparation.conversion} />}

              {suspend && (
                <div className="flex flex-col gap-2 rounded-lg border border-line bg-raised p-3">
                  <h3 className="text-xs font-semibold uppercase tracking-wide text-muted">
                    Suspend preflight
                  </h3>
                  <p className="text-xs text-muted">
                    {suspend.untracked.length} untracked file{suspend.untracked.length === 1 ? '' : 's'} ·{' '}
                    {suspend.nested_repositories.length} nested repositor
                    {suspend.nested_repositories.length === 1 ? 'y' : 'ies'} ·{' '}
                    {suspend.keep_dir_entries.length} keep-dir entr
                    {suspend.keep_dir_entries.length === 1 ? 'y' : 'ies'} (
                    {formatBytes(suspend.keep_dir_bytes)})
                  </p>
                  {suspend.interrupting && (
                    <p className="text-sm text-warn">
                      Confirming interrupts the active turn and checkpoints the session.
                    </p>
                  )}
                  {suspend.children_not_handed_back.length > 0 && (
                    <p className="text-sm text-warn">
                      {suspend.children_not_handed_back.length} sub-agent
                      {suspend.children_not_handed_back.length === 1 ? '' : 's'} will not be handed back.
                    </p>
                  )}
                  {suspend.operations.map((operation, index) => (
                    <p key={`${operation.kind}:${index}`} className="text-sm text-warn">
                      In-flight {operation.kind} in {operation.checkout.repository_id}.
                    </p>
                  ))}
                  {suspend.blockers.map(blocker => (
                    <p key={`${blocker.kind}:${blocker.paths.join(',')}`} className="text-sm text-danger">
                      {blocker.kind}: {blocker.paths.join(', ')} — {blocker.remedy}
                    </p>
                  ))}
                  {suspend.existing_checkpoint && (
                    <p className="text-xs text-muted">
                      Existing checkpoint {suspend.existing_checkpoint.verified ? 'verified' : 'unverified'}
                      {suspend.existing_checkpoint.age_seconds !== undefined
                        ? ` · ${Math.round(suspend.existing_checkpoint.age_seconds / 60)} minutes old`
                        : ''}
                    </p>
                  )}
                </div>
              )}

              <div className="flex flex-col gap-2 rounded-lg border border-line bg-raised p-3">
                <h3 className="text-xs font-semibold uppercase tracking-wide text-muted">
                  Keep-directory transfer
                </h3>
                <p className="text-xs text-muted">
                  Skip large .mj/keep paths that the destination should not copy. Changing this list
                  prepares the move again.
                </p>
                {(preparation.skip_keep_sizes ?? []).map(([path, bytes]) => (
                  <label key={path} className="flex items-center gap-2 text-sm text-fg">
                    <input
                      type="checkbox"
                      checked={skipKeepPaths.includes(path)}
                      onChange={event => toggleSkip(path, event.target.checked)}
                    />
                    Skip <span className="font-mono text-xs">{path}</span>
                    <span className="text-xs text-muted">({formatBytes(bytes)})</span>
                  </label>
                ))}
                <div className="flex flex-wrap items-center gap-2">
                  <input
                    value={skipInput}
                    onChange={event => setSkipInput(event.target.value)}
                    placeholder="relative keep path to skip"
                    spellCheck={false}
                    className="min-w-0 flex-1 rounded-md border border-line bg-bg px-2.5 py-2 font-mono text-xs text-fg"
                  />
                  <button
                    type="button"
                    disabled={!skipInput.trim() || preparing}
                    onClick={addSkip}
                    className="inline-flex min-h-9 items-center rounded-md border border-line px-3 text-sm text-fg hover:bg-surface disabled:opacity-50"
                  >
                    Skip path
                  </button>
                </div>
              </div>

              {preparation.queued_commands.length > 0 && (
                <div className="flex flex-col gap-2 rounded-lg border border-line bg-raised p-3">
                  <h3 className="text-xs font-semibold uppercase tracking-wide text-muted">
                    {preparation.queued_commands.length} queued command
                    {preparation.queued_commands.length === 1 ? '' : 's'}
                  </h3>
                  {preparation.queued_commands.map((item, index) => (
                    <p key={item.command_id || index} className="truncate text-xs text-fg">
                      {index + 1}. {queueText(item)}
                    </p>
                  ))}
                  {preparation.locked_queue ? (
                    <p className="text-xs text-warn">
                      Queue choice locked to {preparation.locked_queue}: the previous destination already
                      admitted it.
                    </p>
                  ) : (
                    <div className="flex flex-col gap-1">
                      <label className="flex items-center gap-2 text-sm text-fg">
                        <input
                          type="radio"
                          name="move-queue"
                          checked={queueChoice === 'discard'}
                          onChange={() => setQueueChoice('discard')}
                        />
                        Discard queued work and start idle
                      </label>
                      <label className="flex items-center gap-2 text-sm text-fg">
                        <input
                          type="radio"
                          name="move-queue"
                          checked={queueChoice === 'start'}
                          onChange={() => setQueueChoice('start')}
                        />
                        Run queued work after the destination is ready
                      </label>
                    </div>
                  )}
                </div>
              )}

              {preparation.active && (
                <label className="flex items-start gap-2 rounded-lg border border-warn/40 bg-warn/10 p-3 text-sm text-fg">
                  <input
                    type="checkbox"
                    checked={acknowledge}
                    onChange={event => setAcknowledge(event.target.checked)}
                    className="mt-0.5"
                  />
                  I understand the active turn is interrupted and the session is checkpointed before it
                  moves.
                </label>
              )}

              {moveError && (
                <p className="text-sm text-danger" role="alert">
                  {moveError}
                </p>
              )}
              <button
                type="button"
                disabled={!canConfirm}
                onClick={() => void confirm()}
                className="inline-flex min-h-10 w-full items-center justify-center gap-2 rounded-lg bg-accent px-4 text-sm font-semibold text-accent-fg disabled:opacity-50"
              >
                {moving && <LoaderCircle className="size-4 animate-spin" aria-hidden />}
                Confirm move
              </button>
            </div>
          )}

          <p className="text-xs text-muted">
            Moving to {workspaceId === detail.workspace_id ? 'another target' : 'another workspace'} keeps
            the session id and transcript. Installed packages and files outside the declared workspace are
            not migrated when the environment is rebuilt.
          </p>
        </>
      )}
    </section>
  );
}
