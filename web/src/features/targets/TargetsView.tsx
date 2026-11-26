import {
  AlertTriangle,
  CheckCircle2,
  CircleHelp,
  Cpu,
  HardDrive,
  MemoryStick,
  RefreshCw,
  Server,
  ShieldAlert,
  XCircle,
} from 'lucide-react';
import { useMemo, useState, type ReactNode } from 'react';
import type {
  AvailabilityVerdict,
  HostContainerCapacity,
  StorageCondition,
  ViewerTarget,
  ViewerTargetCapacity,
  ViewerTargetStorage,
} from '../../api/types';
import { useSessionStore } from '../../stores/useSessionStore';

/** Props the shell passes. Keep this shape when replacing the placeholder. */
export type TargetsViewProps = Record<string, never>;

type Tone = 'accent' | 'ok' | 'warn' | 'danger' | 'muted';

const TONE_BAR: Record<Tone, string> = {
  accent: 'bg-accent',
  ok: 'bg-ok',
  warn: 'bg-warn',
  danger: 'bg-danger',
  muted: 'bg-muted',
};

const TONE_TEXT: Record<Tone, string> = {
  accent: 'text-accent',
  ok: 'text-ok',
  warn: 'text-warn',
  danger: 'text-danger',
  muted: 'text-muted',
};

const STORAGE: Record<StorageCondition, { label: string; tone: Tone; fill: number }> = {
  ok: { label: 'OK', tone: 'ok', fill: 100 },
  low: { label: 'Low', tone: 'warn', fill: 40 },
  full: { label: 'Full', tone: 'danger', fill: 8 },
};

export function formatBytes(bytes: number | undefined): string {
  if (bytes === undefined || !Number.isFinite(bytes) || bytes < 0) return '—';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const rounded = unit === 0 || value >= 10 ? Math.round(value) : Math.round(value * 10) / 10;
  return `${rounded} ${units[unit]}`;
}

function formatTime(epochSeconds: number | undefined): string | undefined {
  if (epochSeconds === undefined) return undefined;
  return new Date(epochSeconds * 1000).toLocaleString();
}

export function Meter({
  label,
  value,
  max,
  detail,
  tone = 'accent',
}: {
  label: string;
  value: number;
  max: number;
  detail?: string;
  tone?: Tone;
}) {
  const percent = max > 0 ? Math.min(100, Math.max(0, Math.round((value / max) * 100))) : 0;
  return (
    <div className="flex flex-col gap-1">
      <div className="flex items-baseline justify-between gap-2 text-xs">
        <span className="text-muted">{label}</span>
        <span className="tabular-nums text-fg">{detail ?? `${percent}%`}</span>
      </div>
      <div
        role="progressbar"
        aria-label={label}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={percent}
        className="h-1.5 overflow-hidden rounded-full bg-raised"
      >
        <div className={`h-full rounded-full ${TONE_BAR[tone]}`} style={{ width: `${percent}%` }} />
      </div>
    </div>
  );
}

function AvailabilityBadge({ verdict }: { verdict: AvailabilityVerdict }) {
  if (verdict.status === 'available') {
    return (
      <span className="inline-flex items-center gap-1 rounded-full bg-ok/10 px-2 py-0.5 text-xs font-medium text-ok">
        <CheckCircle2 className="size-3" aria-hidden /> Available
      </span>
    );
  }
  if (verdict.status === 'missing') {
    return (
      <span
        className="inline-flex items-center gap-1 rounded-full bg-danger/10 px-2 py-0.5 text-xs font-medium text-danger"
        title={verdict.reason}
      >
        <XCircle className="size-3" aria-hidden /> Missing
      </span>
    );
  }
  return (
    <span className="inline-flex items-center gap-1 rounded-full bg-raised px-2 py-0.5 text-xs font-medium text-muted">
      <CircleHelp className="size-3" aria-hidden /> Unknown
    </span>
  );
}

function Chip({ children, tone = 'muted' }: { children: ReactNode; tone?: Tone }) {
  return (
    <span
      className={`inline-flex items-center gap-1 rounded-full border border-line px-2 py-0.5 text-xs font-medium ${TONE_TEXT[tone]}`}
    >
      {children}
    </span>
  );
}

function ContainerCapacity({ capacity }: { capacity: HostContainerCapacity }) {
  const { total, committed, reserved, remaining, complete } = capacity;
  return (
    <div className="flex flex-col gap-2 rounded-lg bg-raised/60 p-2.5">
      <div className="flex items-center justify-between gap-2 text-xs font-semibold uppercase tracking-wide text-muted">
        <span>Container capacity</span>
        {!complete && (
          <span className="flex items-center gap-1 font-normal normal-case text-warn">
            <AlertTriangle className="size-3" aria-hidden /> Incomplete
          </span>
        )}
      </div>
      <Meter
        label="Committed CPUs"
        value={committed.cpus}
        max={total.cpus}
        detail={`${committed.cpus} / ${total.cpus} total`}
        tone={total.cpus > 0 && committed.cpus >= total.cpus ? 'danger' : 'accent'}
      />
      <Meter
        label="Committed RAM"
        value={committed.memory_bytes}
        max={total.memory_bytes}
        detail={`${formatBytes(committed.memory_bytes)} / ${formatBytes(total.memory_bytes)}`}
        tone={
          total.memory_bytes > 0 && committed.memory_bytes >= total.memory_bytes ? 'danger' : 'accent'
        }
      />
      <dl className="grid grid-cols-2 gap-x-4 gap-y-1 text-xs">
        <div className="flex justify-between gap-2">
          <dt className="text-muted">Reserved CPUs</dt>
          <dd className="tabular-nums">{reserved.cpus}</dd>
        </div>
        <div className="flex justify-between gap-2">
          <dt className="text-muted">Remaining CPUs</dt>
          <dd className="tabular-nums">{remaining.cpus}</dd>
        </div>
        <div className="flex justify-between gap-2">
          <dt className="text-muted">Reserved RAM</dt>
          <dd className="tabular-nums">{formatBytes(reserved.memory_bytes)}</dd>
        </div>
        <div className="flex justify-between gap-2">
          <dt className="text-muted">Remaining RAM</dt>
          <dd className="tabular-nums">{formatBytes(remaining.memory_bytes)}</dd>
        </div>
      </dl>
    </div>
  );
}

function StorageMeter({ storage }: { storage: ViewerTargetStorage }) {
  const condition = STORAGE[storage.condition];
  return (
    <div className="flex flex-col gap-1.5 rounded-lg border border-line p-2.5">
      <div className="flex items-start justify-between gap-2">
        <div className="min-w-0">
          <p className="truncate font-mono text-xs text-fg">{storage.mount}</p>
          {storage.machine && <p className="truncate text-xs text-muted">{storage.machine}</p>}
        </div>
        <Chip tone={condition.tone}>
          <HardDrive className="size-3" aria-hidden /> {condition.label}
        </Chip>
      </div>
      <div
        role="progressbar"
        aria-label={`${storage.mount} storage`}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={condition.fill}
        className="h-1.5 overflow-hidden rounded-full bg-raised"
      >
        <div
          className={`h-full rounded-full ${TONE_BAR[condition.tone]}`}
          style={{ width: `${condition.fill}%` }}
        />
      </div>
      <p className="text-xs tabular-nums text-fg">
        {formatBytes(storage.available_bytes)} available
        <span className="text-muted"> · {formatBytes(storage.reserved_bytes)} reserved</span>
      </p>
      {storage.detail && <p className={`text-xs ${TONE_TEXT[condition.tone]}`}>{storage.detail}</p>}
    </div>
  );
}

function HostCapacityCard({
  capacity,
  onRefresh,
  refreshing,
}: {
  capacity: ViewerTargetCapacity;
  onRefresh: (targetId: string) => void;
  refreshing: boolean;
}) {
  return (
    <article
      aria-labelledby={`capacity-${capacity.id}`}
      className="flex flex-col gap-3 rounded-xl border border-line bg-surface p-3.5"
    >
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <h3 id={`capacity-${capacity.id}`} className="truncate text-sm font-semibold">
            {capacity.label}
          </h3>
          <p className="truncate text-xs text-muted">
            {capacity.target_ids.length ? capacity.target_ids.join(', ') : capacity.id}
          </p>
        </div>
        <div className="flex shrink-0 flex-wrap justify-end gap-1">
          {capacity.refreshing && <Chip tone="warn">Refreshing</Chip>}
          {capacity.stale && <Chip tone="warn">Stale</Chip>}
          {capacity.has_error && <Chip tone="danger">Probe failed</Chip>}
        </div>
      </div>

      {(capacity.cpu_percent !== undefined ||
        capacity.memory_used_bytes !== undefined ||
        capacity.memory_total_bytes !== undefined ||
        capacity.logical_cores !== undefined ||
        capacity.virtual_machines !== undefined) && (
        <div className="grid gap-2.5 sm:grid-cols-2">
          {capacity.cpu_percent !== undefined && (
            <Meter
              label="CPU"
              value={capacity.cpu_percent}
              max={100}
              detail={`${capacity.cpu_percent}%`}
              tone={capacity.cpu_percent >= 90 ? 'danger' : capacity.cpu_percent >= 75 ? 'warn' : 'ok'}
            />
          )}
          {capacity.memory_used_bytes !== undefined && capacity.memory_total_bytes !== undefined && (
            <Meter
              label="Memory"
              value={capacity.memory_used_bytes}
              max={capacity.memory_total_bytes}
              detail={`${formatBytes(capacity.memory_used_bytes)} / ${formatBytes(capacity.memory_total_bytes)}`}
              tone={
                capacity.memory_total_bytes > 0 &&
                capacity.memory_used_bytes / capacity.memory_total_bytes >= 0.9
                  ? 'danger'
                  : capacity.memory_total_bytes > 0 &&
                      capacity.memory_used_bytes / capacity.memory_total_bytes >= 0.75
                    ? 'warn'
                    : 'ok'
              }
            />
          )}
        </div>
      )}

      <dl className="grid grid-cols-2 gap-x-4 gap-y-1 text-xs">
        {capacity.logical_cores !== undefined && (
          <div className="flex items-center justify-between gap-2">
            <dt className="text-muted">Logical cores</dt>
            <dd className="tabular-nums">{capacity.logical_cores}</dd>
          </div>
        )}
        {capacity.virtual_machines !== undefined && (
          <div className="flex items-center justify-between gap-2">
            <dt className="text-muted">Virtual machines</dt>
            <dd className="tabular-nums">{capacity.virtual_machines}</dd>
          </div>
        )}
        {capacity.disk_total_bytes !== undefined && (
          <div className="flex items-center justify-between gap-2">
            <dt className="text-muted">Disk</dt>
            <dd className="tabular-nums">{formatBytes(capacity.disk_total_bytes)}</dd>
          </div>
        )}
        {capacity.sampled_at_epoch_seconds !== undefined && (
          <div className="flex items-center justify-between gap-2">
            <dt className="text-muted">Sampled</dt>
            <dd className="tabular-nums">{formatTime(capacity.sampled_at_epoch_seconds)}</dd>
          </div>
        )}
      </dl>

      {capacity.container_capacity && <ContainerCapacity capacity={capacity.container_capacity} />}

      {capacity.storage?.length ? (
        <div className="flex flex-col gap-2">
          <h4 className="text-xs font-semibold uppercase tracking-wide text-muted">Storage</h4>
          <div className="grid gap-2 sm:grid-cols-2">
            {capacity.storage.map(storage => (
              <StorageMeter key={`${storage.machine ?? ''}:${storage.mount}`} storage={storage} />
            ))}
          </div>
        </div>
      ) : null}

      {capacity.target_ids.length > 0 && (
        <div className="flex flex-wrap gap-1.5">
          {capacity.target_ids.map(targetId => (
            <button
              key={targetId}
              type="button"
              disabled={refreshing}
              onClick={() => onRefresh(targetId)}
              className="inline-flex h-8 items-center gap-1.5 rounded-lg border border-line px-2.5 text-xs font-medium text-fg hover:bg-raised disabled:opacity-50"
              aria-label={`Refresh capacity for ${targetId}`}
            >
              <RefreshCw className={`size-3.5 ${refreshing ? 'animate-spin' : ''}`} aria-hidden />
              Refresh capacity
            </button>
          ))}
        </div>
      )}
    </article>
  );
}

function TargetCard({ target }: { target: ViewerTarget }) {
  const limits = target.container_host_limits;
  const remembered = target.remembered_container_size;
  const defaultAllocation =
    target.default_resource_allocation?.kind === 'container'
      ? target.default_resource_allocation
      : undefined;
  return (
    <article
      aria-labelledby={`target-${target.id}`}
      className="flex flex-col gap-3 rounded-xl border border-line bg-surface p-3.5"
    >
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <h3 id={`target-${target.id}`} className="truncate text-sm font-semibold">
            {target.id}
          </h3>
          <p className="text-xs text-muted">{target.kind}</p>
        </div>
        <AvailabilityBadge verdict={target.availability} />
      </div>

      {target.availability.status === 'missing' && (
        <p className="rounded-lg bg-danger/10 p-2 text-xs text-danger" role="alert">
          <span className="font-semibold">Unavailable: </span>
          {target.availability.reason}
        </p>
      )}

      <div className="flex flex-wrap gap-1.5">
        {target.permission_mode !== undefined && (
          <Chip>
            <ShieldAlert className="size-3" aria-hidden /> {target.permission_mode}
          </Chip>
        )}
        {target.yolo && <Chip tone="warn">YOLO</Chip>}
        {target.default_candidate && <Chip tone="accent">Default candidate</Chip>}
        {target.subagents_configured && <Chip>Sub-agents</Chip>}
        {target.oracle_configured && <Chip>Oracle</Chip>}
        {target.runtime_missing && (
          <Chip tone="danger">
            <AlertTriangle className="size-3" aria-hidden /> Runtime missing
          </Chip>
        )}
      </div>

      <dl className="flex flex-col gap-1 text-xs">
        <div className="flex justify-between gap-2">
          <dt className="text-muted">Container host limits</dt>
          <dd className="text-right tabular-nums">
            {limits
              ? `${limits.cpus} CPUs · ${formatBytes(limits.memory_bytes)} RAM`
              : defaultAllocation
                ? `${defaultAllocation.cpus} CPUs · ${formatBytes(defaultAllocation.memory_bytes)} default`
                : 'Not reported'}
          </dd>
        </div>
        <div className="flex justify-between gap-2">
          <dt className="text-muted">Remembered container size</dt>
          <dd className="text-right tabular-nums">
            {remembered
              ? `${remembered.cpus} CPUs · ${formatBytes(remembered.memory_bytes)} RAM`
              : 'None'}
          </dd>
        </div>
        <div className="flex justify-between gap-2">
          <dt className="text-muted">Allocation</dt>
          <dd className="text-right">{target.resource_allocation_kind}</dd>
        </div>
      </dl>

      {target.container_host_limits && target.container_host_limits.memory_bytes > 0 && (
        <div className="flex items-start gap-1.5 text-xs text-muted">
          <MemoryStick className="mt-0.5 size-3.5 shrink-0" aria-hidden />
          <span>
            Containers on this target may use up to {target.container_host_limits.cpus} CPUs and{' '}
            {formatBytes(target.container_host_limits.memory_bytes)} RAM.
          </span>
        </div>
      )}
      {target.container_memory_hard_limit_reason && (
        <p className="text-xs text-muted">{target.container_memory_hard_limit_reason}</p>
      )}
      {target.container_capacity && <ContainerCapacity capacity={target.container_capacity} />}
      {target.requires_project_directory && (
        <p className="flex items-center gap-1.5 text-xs text-muted">
          <Server className="size-3.5" aria-hidden /> Requires a project directory.
        </p>
      )}
    </article>
  );
}

function AvailabilityNotes() {
  const notes = useSessionStore(state => state.meta?.target_availability_notes);
  if (!notes?.length) return null;
  return (
    <section aria-label="Availability notes" className="rounded-xl border border-line bg-surface p-3.5">
      <h2 className="mb-2 text-sm font-semibold">Availability notes</h2>
      <ul className="flex flex-col gap-2">
        {notes.map((note, index) => (
          <li key={`${note.text}-${index}`} className="flex items-start gap-2 text-xs">
            <span
              className={`mt-0.5 shrink-0 rounded-full px-1.5 py-0.5 text-[10px] font-semibold uppercase ${
                note.routine ? 'bg-raised text-muted' : 'bg-warn/10 text-warn'
              }`}
            >
              {note.routine ? 'Routine' : 'Attention'}
            </span>
            <span>
              <span className="text-fg">{note.text}</span>
              <span className="mt-0.5 block text-muted">{note.full}</span>
            </span>
          </li>
        ))}
      </ul>
    </section>
  );
}

export default function TargetsView(_props: TargetsViewProps) {
  const meta = useSessionStore(state => state.meta);
  const dispatchAction = useSessionStore(state => state.dispatchAction);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const targets = meta?.targets ?? [];
  const capacity = meta?.capacity ?? [];

  const counts = useMemo(() => {
    const available = targets.filter(target => target.availability.status === 'available').length;
    const missing = targets.filter(target => target.availability.status === 'missing').length;
    return { available, missing };
  }, [targets]);

  const run = async (key: string, action: Parameters<typeof dispatchAction>[0]) => {
    setBusy(key);
    setError(null);
    try {
      await dispatchAction(action);
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
    } finally {
      setBusy(null);
    }
  };

  if (!meta) {
    return (
      <section className="mx-auto flex max-w-3xl flex-col gap-3 p-4" aria-busy="true">
        <h2 className="text-lg font-semibold tracking-tight">Targets</h2>
        <p className="text-sm text-muted" role="status">
          Loading targets…
        </p>
      </section>
    );
  }

  return (
    <section className="mx-auto flex w-full max-w-4xl flex-col gap-3 p-3 pb-6 sm:p-4">
      <header className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h2 className="text-lg font-semibold tracking-tight">Targets</h2>
          <p className="text-xs text-muted">
            {targets.length} target{targets.length === 1 ? '' : 's'} · {counts.available} available
            {counts.missing > 0 ? ` · ${counts.missing} missing` : ''}
          </p>
        </div>
        <button
          type="button"
          disabled={busy === 'check'}
          onClick={() => void run('check', { action: 'check-availability' })}
          className="inline-flex h-10 items-center gap-1.5 rounded-lg bg-accent px-3 text-sm font-semibold text-accent-fg disabled:opacity-50"
        >
          <RefreshCw className={`size-4 ${busy === 'check' ? 'animate-spin' : ''}`} aria-hidden />
          Check availability
        </button>
      </header>

      {error && (
        <p role="alert" className="rounded-lg bg-danger/10 p-2.5 text-sm text-danger">
          {error}
        </p>
      )}

      <AvailabilityNotes />

      <section aria-labelledby="host-capacity-heading" className="flex flex-col gap-2">
        <h2 id="host-capacity-heading" className="flex items-center gap-1.5 text-sm font-semibold">
          <Cpu className="size-4 text-muted" aria-hidden /> Host capacity
        </h2>
        {capacity.length === 0 ? (
          <p className="rounded-xl border border-line bg-surface p-3.5 text-sm text-muted">
            No host capacity has been sampled yet.
          </p>
        ) : (
          <div className="grid gap-3 lg:grid-cols-2">
            {capacity.map(host => (
              <HostCapacityCard
                key={host.id}
                capacity={host}
                refreshing={busy === `capacity:${host.id}`}
                onRefresh={targetId =>
                  void run(`capacity:${host.id}`, { action: 'refresh-capacity', target_id: targetId })
                }
              />
            ))}
          </div>
        )}
      </section>

      <section aria-labelledby="target-list-heading" className="flex flex-col gap-2">
        <h2 id="target-list-heading" className="flex items-center gap-1.5 text-sm font-semibold">
          <Server className="size-4 text-muted" aria-hidden /> Targets
        </h2>
        {targets.length === 0 ? (
          <p className="rounded-xl border border-line bg-surface p-3.5 text-sm text-muted">
            No targets are configured.
          </p>
        ) : (
          <div className="grid gap-3 lg:grid-cols-2">
            {targets.map(target => (
              <TargetCard key={target.id} target={target} />
            ))}
          </div>
        )}
      </section>
    </section>
  );
}
