// The inbox asks one question: which sessions need this person, in what order?
// The daemon publishes the facts; this module only orders and addresses them.

import type {
  NotificationDismissal,
  NotificationMessageSelector,
  NotificationStackEntry,
  SessionAttention,
  SessionRow,
} from '../../api/types';

export type InboxBadge = 'needs_input' | 'important' | 'failed' | 'unreachable' | 'attention';

export interface InboxItem {
  row: SessionRow;
  badge: InboxBadge;
  /** Server time the card or message was raised. */
  timestamp: number;
}

export interface AcceptedDismissal {
  target: string;
  card: boolean;
}

const BADGE_RANK: Record<InboxBadge, number> = {
  needs_input: 0,
  important: 1,
  failed: 2,
  unreachable: 2,
  attention: 3,
};

function isFailedAttention(attention: SessionAttention | undefined): boolean {
  return attention === 'failed' || attention === 'unreachable';
}

/** A session whose failure needs a person, not merely a status row. */
export function isFailedSession(row: SessionRow): boolean {
  return isFailedAttention(row.attention);
}

/**
 * The dismissal the daemon currently publishes. A card owns its own dismissal;
 * a failed session without a card falls back to the row's dismissal.
 */
export function publishedDismissal(row: SessionRow): NotificationDismissal | null {
  if (row.notification_card) return row.notification_card.dismissal;
  if (!isFailedSession(row)) return null;
  return row.notification_dismissal ?? null;
}

/** The target a Dismiss button would send right now. */
export function dismissalTarget(row: SessionRow): NotificationDismissal | null {
  return row.notification_card
    ? row.notification_card.dismissal
    : row.notification_dismissal ?? null;
}

export function acceptedMatches(
  row: SessionRow,
  accepted: Record<string, AcceptedDismissal>,
): boolean {
  const seen = accepted[row.id];
  if (!seen) return false;
  return (
    seen.card === Boolean(row.notification_card) &&
    seen.target === JSON.stringify(publishedDismissal(row))
  );
}

/** `notifications_ignored` is a detail-only field; summary rows may omit it. */
export function notificationsIgnored(row: SessionRow): boolean {
  return (row as { notifications_ignored?: boolean }).notifications_ignored === true;
}

export function sessionTimestamp(row: SessionRow): number {
  return row.last_message_at_ms ?? row.last_activity_at_ms ?? Date.parse(row.updated_at) ?? 0;
}

function inboxBadge(row: SessionRow): InboxBadge {
  const card = row.notification_card;
  if (card?.badge === 'needs_input') return 'needs_input';
  if (card?.badge === 'important') return 'important';
  if (row.attention === 'needs_input') return 'needs_input';
  if (row.attention === 'important') return 'important';
  if (row.attention === 'failed') return 'failed';
  if (row.attention === 'unreachable') return 'unreachable';
  return 'attention';
}

export function badgeLabel(badge: InboxBadge): string {
  switch (badge) {
    case 'needs_input':
      return 'Needs input';
    case 'important':
      return 'Important';
    case 'failed':
      return 'Failed';
    case 'unreachable':
      return 'Unreachable';
    default:
      return 'Attention';
  }
}

export function badgeTone(badge: InboxBadge): string {
  switch (badge) {
    case 'needs_input':
      return 'border-warn/50 bg-warn/10 text-warn';
    case 'important':
      return 'border-accent/50 bg-accent/10 text-accent';
    case 'failed':
    case 'unreachable':
      return 'border-danger/50 bg-danger/10 text-danger';
    default:
      return 'border-line bg-raised text-muted';
  }
}

/**
 * Every session needing attention, highest priority first, then newest.
 * Ignored and already-dismissed notifications do not appear.
 */
export function inboxItems(
  rows: Iterable<SessionRow>,
  accepted: Record<string, AcceptedDismissal> = {},
): InboxItem[] {
  const items: InboxItem[] = [];
  for (const row of rows) {
    if (notificationsIgnored(row)) continue;
    const card = row.notification_card;
    const attention = row.attention;
    const include =
      Boolean(card) ||
      attention === 'needs_input' ||
      attention === 'important' ||
      isFailedAttention(attention);
    if (!include) continue;
    if (acceptedMatches(row, accepted)) continue;
    items.push({
      row,
      badge: inboxBadge(row),
      timestamp: card?.since_ms ?? sessionTimestamp(row),
    });
  }
  return items.sort(
    (left, right) =>
      BADGE_RANK[left.badge] - BADGE_RANK[right.badge] ||
      right.timestamp - left.timestamp ||
      String(left.row.title || '').localeCompare(String(right.row.title || '')) ||
      left.row.id.localeCompare(right.row.id),
  );
}

/** The lazy full-message selector for one stack entry. */
export function entrySelector(entry: NotificationStackEntry): NotificationMessageSelector {
  return typeof entry.message === 'object' && 'found' in entry.message
    ? { message_id: entry.message.found }
    : { through: entry.completed_ordinal };
}

/** The lazy selector for a card's live form, when the daemon published its identity. */
export function formSelector(row: SessionRow): NotificationMessageSelector | null {
  const card = row.notification_card;
  const form = card?.form;
  if (!card || !form || form.raised_at_ms == null) return null;
  return {
    holder: card.holder ?? row.id,
    request_id: form.request_id,
    raised_at_ms: form.raised_at_ms,
  };
}

export function relativeTime(timestamp: number | undefined | null, now: number): string {
  if (!timestamp) return '';
  const elapsed = Math.max(0, now - timestamp);
  const seconds = Math.floor(elapsed / 1000);
  if (seconds < 45) return 'just now';
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  if (days < 7) return `${days}d ago`;
  return new Date(timestamp).toLocaleDateString();
}

/** The destination for Open: Resume a suspended session, else its conversation. */
export function openRouteFor(row: SessionRow): 'conversation' | 'resume' {
  if (row.lifecycle === 'suspended') return 'resume';
  if (!row.capabilities.open && row.lifecycle !== 'live') return 'resume';
  return 'conversation';
}
