import type { RelayCommand, TurnControlKind, ViewerSession } from '../../api/types';

/**
 * What the composer offers for the turn the session currently shows.
 *
 * The worker publishes `turn_controls`; the browser forwards the published
 * command and never builds a command for another turn. Only a session that
 * publishes nothing falls back to the untargeted `interrupt-turn` action.
 */
export interface TurnControlView {
  /** Present when the worker published controls for this session. */
  published: boolean;
  pending: boolean;
  uncertain: boolean;
  unavailable: boolean;
  message: string;
  primary: { kind: TurnControlKind; label: string; command: RelayCommand } | null;
  /** The published interrupt for the displayed turn, or null. */
  interrupt: RelayCommand | null;
  /** Untargeted `interrupt-turn`, only for sessions that publish no controls. */
  legacyInterrupt: boolean;
}

const PRIMARY_LABELS: Record<TurnControlKind, string> = {
  steer: 'Steer',
  apply_queued: 'Apply queued',
  interrupt: 'Interrupt',
};

export function turnControlView(session: ViewerSession): TurnControlView {
  const published = session.turn_controls;
  const held = session.verdict.move_queue_held || session.transitioning || session.verdict.transition;
  if (!published) {
    return {
      published: false,
      pending: false,
      uncertain: false,
      unavailable: false,
      message: '',
      primary: null,
      interrupt: null,
      legacyInterrupt: !held && Boolean(session.capabilities.interrupt_turn) && session.verdict.interrupt,
    };
  }
  const primary = published.primary;
  // A primary that is itself an interruption is the one Interrupt button.
  const interrupt = held ? null : (published.interrupt ?? (primary?.kind === 'interrupt' ? primary.command : null));
  return {
    published: true,
    pending: published.status === 'pending',
    uncertain: published.status === 'unconfirmed',
    unavailable: published.status === 'unavailable',
    message: published.message ?? '',
    primary:
      !held && primary && primary.kind !== 'interrupt'
        ? { kind: primary.kind, label: PRIMARY_LABELS[primary.kind] ?? primary.kind, command: primary.command }
        : null,
    interrupt,
    legacyInterrupt: false,
  };
}

/** The turn a published command targets, so a retry only offers the turn the notice is about. */
export function turnControlTarget(command: RelayCommand | null): string | null {
  if (!command) return null;
  const data = (command as { data?: Record<string, unknown> }).data;
  const target = data?.target as { kind?: string; command_id?: string; start_ordinal?: number; completed_command_id?: string } | undefined;
  const commandId = target?.command_id ?? (data?.active_prompt_id as string | undefined);
  if (commandId) return `command:${commandId}`;
  if (target?.kind === 'quota_recovery') return `quota:${target.completed_command_id}`;
  return target?.kind === 'autonomous' ? `autonomous:${target.start_ordinal}` : null;
}
