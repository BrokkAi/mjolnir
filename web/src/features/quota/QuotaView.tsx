import {
  AlertTriangle,
  CheckCircle2,
  CircleHelp,
  Gauge,
  RefreshCw,
  Timer,
  XCircle,
} from 'lucide-react';
import { useState } from 'react';
import type {
  AvailabilityVerdict,
  ResetCountdownStyle,
  ViewerProfile,
  ViewerQuota,
  ViewerQuotaWindow,
} from '../../api/types';
import { useSessionStore } from '../../stores/useSessionStore';

/** Props the shell passes. Keep this shape when replacing the placeholder. */
export type QuotaViewProps = Record<string, never>;

function clampPercent(value: number): number {
  return Math.min(100, Math.max(0, value));
}

function formatPercent(value: number): string {
  const rounded = Math.round(value * 10) / 10;
  return Number.isInteger(rounded) ? `${rounded}%` : `${rounded.toFixed(1)}%`;
}

/** Countdown text in the server's style: compact hours for 5-hour windows, days for long ones. */
export function formatResetCountdown(style: ResetCountdownStyle, seconds: number): string {
  const total = Math.max(0, Math.floor(seconds));
  if (total === 0) return 'now';
  const days = Math.floor(total / 86_400);
  const hours = Math.floor((total % 86_400) / 3_600);
  const minutes = Math.floor((total % 3_600) / 60);
  if (style === 'long') {
    if (days > 0) return `${days}d ${hours}h`;
    if (hours > 0) return `${hours}h ${minutes}m`;
    if (minutes > 0) return `${minutes}m`;
    return 'less than a minute';
  }
  if (hours > 0) return `${hours}h ${minutes}m`;
  if (minutes > 0) return `${minutes}m`;
  return 'less than a minute';
}

function formatTimestamp(epochSeconds: number | undefined): string | undefined {
  if (epochSeconds === undefined) return undefined;
  return new Date(epochSeconds * 1000).toLocaleString();
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

function QuotaWindow({ window, nowMs }: { window: ViewerQuotaWindow; nowMs: number }) {
  const used =
    window.percent_used !== undefined
      ? clampPercent(window.percent_used)
      : window.fable_remaining_percent !== undefined
        ? clampPercent(100 - window.fable_remaining_percent)
        : undefined;
  const remaining = used === undefined ? undefined : 100 - used;
  const tone = used === undefined ? 'bg-muted' : used >= 90 ? 'bg-danger' : used >= 75 ? 'bg-warn' : 'bg-accent';
  const resetSeconds =
    window.resets_at_epoch_seconds !== undefined
      ? window.resets_at_epoch_seconds - nowMs / 1000
      : undefined;
  const resetTimestamp = formatTimestamp(window.resets_at_epoch_seconds) ?? window.resets_at;

  return (
    <li className="flex flex-col gap-2 rounded-lg border border-line bg-bg/40 p-2.5">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="flex min-w-0 flex-wrap items-center gap-1.5">
          <span className="truncate text-xs font-semibold text-fg">{window.label}</span>
          {window.pool && (
            <span className="rounded-full border border-line px-2 py-0.5 text-[10px] font-medium text-muted">
              {window.pool}
            </span>
          )}
        </div>
        <div className="flex flex-wrap items-center justify-end gap-1.5">
          {window.projects_exhaustion_before_reset && (
            <span className="inline-flex items-center gap-1 rounded-full bg-warn/10 px-2 py-0.5 text-[10px] font-semibold text-warn">
              <AlertTriangle className="size-3" aria-hidden /> Projects may exhaust before reset
            </span>
          )}
          {window.banked_resets !== undefined && window.banked_resets > 0 && (
            <span className="rounded-full bg-accent/10 px-2 py-0.5 text-[10px] font-semibold text-accent">
              {window.banked_resets} banked reset{window.banked_resets === 1 ? '' : 's'}
            </span>
          )}
        </div>
      </div>

      {used !== undefined && (
        <>
          <div
            role="progressbar"
            aria-label={`${window.label} usage`}
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={Math.round(used)}
            className="h-2 overflow-hidden rounded-full bg-raised"
          >
            <div className={`h-full rounded-full ${tone}`} style={{ width: `${used}%` }} />
          </div>
          <div className="flex items-baseline justify-between gap-2 text-xs tabular-nums">
            <span className="text-fg">
              {window.percent_used !== undefined
                ? `${formatPercent(used)} used`
                : `${formatPercent(remaining ?? 0)} remaining`}
            </span>
            {remaining !== undefined && window.percent_used === undefined && (
              <span className="text-muted">{formatPercent(used)} used</span>
            )}
          </div>
        </>
      )}

      <div className="flex flex-wrap items-center justify-between gap-2 text-xs">
        <span className="inline-flex items-center gap-1 text-muted">
          <Timer className="size-3.5" aria-hidden />
          {resetSeconds !== undefined ? (
            <>
              {resetSeconds <= 0 ? 'Reset due' : `Resets in ${formatResetCountdown(window.reset_countdown_style, resetSeconds)}`}
            </>
          ) : (
            'Reset time unavailable'
          )}
        </span>
        {resetTimestamp && (
          <time
            className="text-muted"
            dateTime={
              window.resets_at_epoch_seconds !== undefined
                ? new Date(window.resets_at_epoch_seconds * 1000).toISOString()
                : resetTimestamp
            }
          >
            {resetTimestamp}
          </time>
        )}
      </div>
    </li>
  );
}

function QuotaSection({
  quota,
  nowMs,
  staleLabel,
}: {
  quota: ViewerQuota;
  nowMs: number;
  staleLabel?: string;
}) {
  const rateLimitedUntil = formatTimestamp(quota.rate_limited_until_epoch_seconds);
  return (
    <div className="flex flex-col gap-2.5">
      <div className="flex flex-wrap items-start justify-between gap-2">
        <p className="text-sm text-fg">{quota.summary}</p>
        <div className="flex flex-wrap justify-end gap-1">
          {quota.stale && (
            <span className="rounded-full bg-warn/10 px-2 py-0.5 text-[10px] font-semibold text-warn">
              Stale
            </span>
          )}
          {quota.has_error && (
            <span className="rounded-full bg-danger/10 px-2 py-0.5 text-[10px] font-semibold text-danger">
              Last check failed
            </span>
          )}
          {quota.rate_limited_until_epoch_seconds !== undefined && (
            <span className="rounded-full bg-warn/10 px-2 py-0.5 text-[10px] font-semibold text-warn">
              Rate limited{rateLimitedUntil ? ` until ${rateLimitedUntil}` : ''}
            </span>
          )}
        </div>
      </div>

      {quota.windows?.length ? (
        <ul aria-label="Quota windows" className="flex flex-col gap-2">
          {quota.windows.map((window, index) => (
            <QuotaWindow key={`${window.label}-${window.pool ?? ''}-${index}`} window={window} nowMs={nowMs} />
          ))}
        </ul>
      ) : (
        <p className="text-xs text-muted">No quota windows reported.</p>
      )}

      <p className="text-xs text-muted">
        {staleLabel ?? 'Refreshed'} {formatTimestamp(quota.refreshed_at_epoch_seconds) ?? 'unknown'}
        {quota.next_check_at_epoch_seconds !== undefined && (
          <> · next check {formatTimestamp(quota.next_check_at_epoch_seconds)}</>
        )}
      </p>
    </div>
  );
}

function ProfileCard({
  profile,
  nowMs,
  refreshing,
  onRefresh,
}: {
  profile: ViewerProfile;
  nowMs: number;
  refreshing: boolean;
  onRefresh: () => void;
}) {
  return (
    <article
      aria-labelledby={`profile-${profile.id}`}
      className="flex flex-col gap-3 rounded-xl border border-line bg-surface p-3.5"
    >
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <h3 id={`profile-${profile.id}`} className="truncate text-sm font-semibold">
            {profile.id}
          </h3>
          <p className="text-xs text-muted">{profile.harness_kind}</p>
        </div>
        <div className="flex shrink-0 flex-col items-end gap-1">
          <AvailabilityBadge verdict={profile.availability} />
          <button
            type="button"
            onClick={onRefresh}
            disabled={refreshing}
            aria-label={`Refresh quota for ${profile.id}`}
            className="inline-flex h-8 items-center gap-1.5 rounded-lg border border-line px-2.5 text-xs font-medium text-fg hover:bg-raised disabled:opacity-50"
          >
            <RefreshCw className={`size-3.5 ${refreshing ? 'animate-spin' : ''}`} aria-hidden />
            Refresh
          </button>
        </div>
      </div>

      {profile.availability.status === 'missing' && (
        <p className="rounded-lg bg-danger/10 p-2 text-xs text-danger" role="alert">
          <span className="font-semibold">Unavailable: </span>
          {profile.availability.reason}
        </p>
      )}

      {profile.quota_pools?.length ? (
        <div className="flex flex-wrap gap-1.5" aria-label="Quota pools">
          {profile.quota_pools.map(pool => (
            <span
              key={pool}
              className="rounded-full border border-line px-2 py-0.5 text-[10px] font-medium text-muted"
            >
              {pool}
            </span>
          ))}
        </div>
      ) : null}

      {profile.quota ? (
        <QuotaSection quota={profile.quota} nowMs={nowMs} />
      ) : (
        <p className="text-xs text-muted">No quota has been reported for this profile.</p>
      )}
    </article>
  );
}

export default function QuotaView(_props: QuotaViewProps) {
  const meta = useSessionStore(state => state.meta);
  const dispatchAction = useSessionStore(state => state.dispatchAction);
  const [busyProfile, setBusyProfile] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const profiles = meta?.profiles ?? [];
  const nowMs = meta?.server_time_ms ?? Date.now();
  const refreshed = profiles.filter(profile => profile.quota?.refreshed_at_epoch_seconds).length;

  const refresh = async (profileId: string) => {
    setBusyProfile(profileId);
    setError(null);
    try {
      await dispatchAction({ action: 'refresh-quota', profile_id: profileId });
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
    } finally {
      setBusyProfile(null);
    }
  };

  if (!meta) {
    return (
      <section className="mx-auto flex max-w-3xl flex-col gap-3 p-4" aria-busy="true">
        <h2 className="text-lg font-semibold tracking-tight">Quota</h2>
        <p role="status" className="text-sm text-muted">
          Loading quota…
        </p>
      </section>
    );
  }

  return (
    <section className="mx-auto flex w-full max-w-4xl flex-col gap-3 p-3 pb-6 sm:p-4">
      <header className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h2 className="flex items-center gap-1.5 text-lg font-semibold tracking-tight">
            <Gauge className="size-5 text-muted" aria-hidden /> Quota
          </h2>
          <p className="text-xs text-muted">
            {profiles.length} profile{profiles.length === 1 ? '' : 's'} · {refreshed} reporting quota
          </p>
        </div>
      </header>

      {error && (
        <p role="alert" className="rounded-lg bg-danger/10 p-2.5 text-sm text-danger">
          {error}
        </p>
      )}

      {profiles.length === 0 ? (
        <p className="rounded-xl border border-line bg-surface p-3.5 text-sm text-muted">
          No profiles are configured.
        </p>
      ) : (
        <div className="grid gap-3 lg:grid-cols-2">
          {profiles.map(profile => (
            <ProfileCard
              key={profile.id}
              profile={profile}
              nowMs={nowMs}
              refreshing={busyProfile === profile.id}
              onRefresh={() => void refresh(profile.id)}
            />
          ))}
        </div>
      )}
    </section>
  );
}
