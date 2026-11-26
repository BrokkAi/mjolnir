import type { ReactNode } from 'react';
import { isSummaryRow, type SessionRow } from '../../api/types';
import { useSessionStore } from '../../stores/useSessionStore';
import { OverlaySheet } from './OverlaySheet';
import { formatBytes } from './format';

/**
 * Read-only facts about the session: where it runs, what it consumes, and
 * every notice that explains a broken state. Nothing here mutates anything;
 * lifecycle actions live in the header's overflow menu.
 */

export interface SessionInfoSheetProps {
  session: SessionRow;
  onClose: () => void;
}

function Row({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="grid grid-cols-[8.5rem_minmax(0,1fr)] gap-3 border-b border-line/60 py-2 last:border-b-0">
      <dt className="text-[11px] uppercase tracking-wide text-muted">{label}</dt>
      <dd className="min-w-0 break-words text-[13px] text-fg">{children}</dd>
    </div>
  );
}

export function SessionInfoSheet({ session, onClose }: SessionInfoSheetProps) {
  const meta = useSessionStore(state => state.meta);
  const full = isSummaryRow(session) ? null : session;
  const profile = meta?.profiles.find(item => item.id === session.profile_id);
  const target = meta?.targets.find(item => item.id === session.target_id);
  const bundle = meta?.bundles.find(item => item.id === session.bundle_id);
  const workspace = meta?.workspaces?.find(item => item.id === session.workspace_id);

  const resource = session.resource_allocation;
  const allocation = resource
    ? resource.kind === 'container'
      ? `${resource.cpus} CPU · ${formatBytes(resource.memory_bytes)}`
      : `${resource.instance_type} · ${resource.vcpus} vCPU · ${formatBytes(resource.memory_bytes)}`
    : 'Default for the target';

  const notices: { label: string; text: string; danger?: boolean }[] = [];
  if (session.configuration_issue) {
    notices.push({ label: 'Configuration', text: session.configuration_issue, danger: true });
  }
  if (session.launch_error) notices.push({ label: 'Launch', text: session.launch_error, danger: true });
  if (session.storage_problem) notices.push({ label: 'Storage', text: session.storage_problem, danger: true });
  if (full?.review_unavailable) {
    notices.push({ label: 'Review', text: full.review_unavailable, danger: true });
  }
  if (full?.move_recovery?.failure_notice) {
    notices.push({ label: 'Move', text: full.move_recovery.failure_notice, danger: true });
  }
  if (full?.quota_recovery) {
    notices.push({ label: 'Quota', text: full.quota_recovery.notice, danger: true });
  }
  if (session.has_error) {
    notices.push({ label: 'Session', text: 'The session reported an error.', danger: true });
  }

  return (
    <OverlaySheet title="Session info" subtitle={session.title || session.id} onClose={onClose} testId="session-info-sheet">
      <dl className="divide-y divide-line/60">
        <Row label="Session">{session.id}</Row>
        <Row label="Lifecycle">
          {session.lifecycle}
          {session.transitioning && <span className="ml-1 text-warn">(transitioning)</span>}
          {session.chat_phase !== 'idle' && <span className="ml-1 text-muted">· chat {session.chat_phase}</span>}
        </Row>
        <Row label="Attention">{session.attention}</Row>
        <Row label="Target">
          {target ? `${target.id} (${target.kind})` : session.target_id}
        </Row>
        <Row label="Profile">
          {profile ? `${profile.id} (${profile.harness_kind})` : session.profile_id}
        </Row>
        <Row label="Bundle">{bundle ? bundle.primary_repository : session.bundle_id}</Row>
        <Row label="Workspace">{workspace?.name || session.workspace_id || '—'}</Row>
        <Row label="Location">{session.display_location || '—'}</Row>
        <Row label="Allocation">{allocation}</Row>
        {full?.branch && (
          <Row label="Branch">
            <span className="font-mono text-[12px]">{full.branch}</span>
          </Row>
        )}
        {full?.at && (
          <Row label="At">
            <span className="font-mono text-[12px]">{full.at}</span>
          </Row>
        )}
        {full?.base && (
          <Row label="Base">
            <span className="font-mono text-[12px]">{full.base}</span>
            {full.no_later_history && <span className="ml-1 text-muted">(no later history)</span>}
          </Row>
        )}
        {full?.publication_state && <Row label="Publication">{full.publication_state}</Row>}
        {full?.managed_checkout_kind && <Row label="Checkout">{full.managed_checkout_kind}</Row>}
        <Row label="Checkpoint">{full?.has_checkpoint ? 'Present' : 'None'}</Row>
        <Row label="Created">{session.created_at}</Row>
        <Row label="Updated">{session.updated_at}</Row>
      </dl>

      {full && full.additional_mounts && full.additional_mounts.length > 0 && (
        <section className="mt-4">
          <h3 className="mb-1 text-[11px] uppercase tracking-wide text-muted">Additional mounts</h3>
          <ul className="space-y-1">
            {full.additional_mounts.map((mount, index) => (
              <li
                key={`${mount.source}:${mount.destination}:${index}`}
                className="flex min-w-0 items-center gap-2 rounded-md border border-line bg-surface px-2 py-1 font-mono text-[11.5px]"
              >
                <span className="min-w-0 flex-1 truncate">{mount.source}</span>
                <span className="shrink-0 text-muted">→</span>
                <span className="min-w-0 flex-1 truncate">{mount.destination}</span>
                <span className="shrink-0 rounded bg-raised px-1.5 py-0.5 text-[10px] text-muted">
                  {mount.access ?? (mount.read_only ? 'ro' : 'cow')}
                </span>
              </li>
            ))}
          </ul>
        </section>
      )}

      {notices.length > 0 && (
        <section className="mt-4 space-y-1.5">
          <h3 className="text-[11px] uppercase tracking-wide text-muted">Notices</h3>
          {notices.map((notice, index) => (
            <div
              key={`${notice.label}:${index}`}
              className={`rounded-md border px-2.5 py-1.5 ${
                notice.danger ? 'border-danger/40 bg-danger/10' : 'border-line bg-surface'
              }`}
            >
              <p className={`text-[11px] font-medium ${notice.danger ? 'text-danger' : 'text-muted'}`}>
                {notice.label}
              </p>
              <p className="whitespace-pre-wrap break-words text-[12.5px] text-fg">{notice.text}</p>
            </div>
          ))}
        </section>
      )}
    </OverlaySheet>
  );
}
