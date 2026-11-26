// Derived facts about the open conversation. Both the scroll engine and the
// feed ask here, so "is a reply streaming" and "which local prompts are still
// unrepresented" each have one definition.

import type { SessionRow } from '../../api/types';
import {
  entryKey,
  type CachedTranscript,
  type PendingLocalPrompt,
} from '../../stores/useTranscriptStore';

/**
 * Optimistic prompts the transcript does not yet represent. A server row that
 * carries the same `command_id` replaces the local row, so deduping here keeps
 * one definition of "already acknowledged".
 */
export function pendingLocalPrompts(cached: CachedTranscript): PendingLocalPrompt[] {
  if (cached.pendingLocalPrompts.length === 0) return [];
  const answered = new Set<string>();
  for (const id of cached.orderedIds) {
    const commandId = cached.entriesById[id]?.command_id;
    if (commandId) answered.add(commandId);
  }
  return cached.pendingLocalPrompts.filter(prompt => !answered.has(prompt.commandId));
}

/**
 * Changes whenever the rendered transcript may have grown: rows added, the
 * newest row revised, streaming text extended, or a local prompt added.
 */
export function transcriptContentVersion(cached: CachedTranscript): string {
  const lastId = cached.orderedIds[cached.orderedIds.length - 1];
  const last = lastId ? cached.entriesById[lastId] : undefined;
  const lastChars = last ? last.lines.reduce((total, line) => total + line.length, 0) : 0;
  return [
    cached.orderedIds.length,
    cached.latestSeq,
    last?.updated_seq ?? 0,
    lastChars,
    pendingLocalPrompts(cached).length,
  ].join(':');
}

/**
 * The assistant reply currently being written, if any. Only the newest row can
 * be streaming, and only while the session says a turn is running; a reply
 * that merely happens to be last while the session is idle is complete.
 */
export function selectStreamingEntryKey(
  session: SessionRow | undefined,
  cached: CachedTranscript | undefined,
): string | null {
  if (!session || !cached || !cached.hasLoaded || cached.orderedIds.length === 0) return null;
  if (session.detail === false || session.transitioning) return null;
  const activity = session.activity_state?.state;
  const working = session.verdict.working || activity === 'turn' || activity === 'tool';
  if (!working) return null;
  const lastId = cached.orderedIds[cached.orderedIds.length - 1];
  const last = lastId ? cached.entriesById[lastId] : undefined;
  if (!last) return null;
  if (last.role !== 'agent' && last.role !== 'message') return null;
  return entryKey(last);
}

/** A read receipt may only be sent while the session is open and not moving. */
export function sessionCanMarkRead(session: SessionRow | undefined): boolean {
  return Boolean(session && session.capabilities.open && !session.transitioning);
}
