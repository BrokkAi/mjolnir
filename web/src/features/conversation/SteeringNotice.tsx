import { useState } from 'react';
import { AlertTriangle } from 'lucide-react';
import type { ViewerSession } from '../../api/types';
import { useSessionStore } from '../../stores/useSessionStore';
import { describeError } from './format';
import { turnControlTarget, turnControlView } from './turnControls';

/**
 * Explains a steering delivery that did not finish and offers the recovery the
 * worker allows: interrupt the same turn, retry the queued prompt, or remove it.
 * "Unconfirmed" means delivery may already have happened, so the notice says
 * that a retry can deliver twice.
 */
export function SteeringNotice({ session }: { session: ViewerSession }) {
  const [dismissed, setDismissed] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const steering = session.steering;
  const controls = turnControlView(session);
  const held = session.verdict.move_queue_held || session.transitioning || session.verdict.transition;

  const explanation = !held && controls.message && !controls.pending && !controls.uncertain ? controls.message : '';
  const unfinished = steering && (steering.status === 'unconfirmed' || steering.status === 'failed');
  const signature = steering ? `${steering.command_id}:${steering.status}` : '';
  const showRecovery = Boolean(unfinished) && dismissed !== signature;
  if (!explanation && !showRecovery && !error) return null;

  const run = async (work: () => Promise<unknown>) => {
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      await work();
    } catch (failure) {
      setError(describeError(failure));
    } finally {
      setBusy(false);
    }
  };

  const uncertain = steering?.status === 'unconfirmed';
  const sameTurn =
    controls.interrupt &&
    steering?.active_prompt_id &&
    turnControlTarget(controls.interrupt) === `command:${steering.active_prompt_id}`;

  return (
    <div
      role="status"
      data-testid="steering-notice"
      className="mb-2 rounded-lg border border-warn/40 bg-warn/10 px-2.5 py-1.5 text-[12px] text-fg"
    >
      {explanation && <p className="text-muted">{explanation}</p>}
      {showRecovery && steering && (
        <>
          <p className="flex items-start gap-1.5">
            <AlertTriangle className="mt-0.5 size-3.5 shrink-0 text-warn" aria-hidden />
            <span className="min-w-0 break-words">
              {steering.message || 'Steering failed; the prompt remains queued.'}
              {uncertain && ' Delivery may already have occurred; retrying can deliver it twice.'}
            </span>
          </p>
          {!held && (
            <div className="mt-1.5 flex flex-wrap gap-1.5">
              <button type="button" onClick={() => setDismissed(signature)} className={buttonClass}>
                Keep queued
              </button>
              {sameTurn && controls.interrupt ? (
                <button
                  type="button"
                  disabled={busy}
                  title="Interrupt this turn; queued input stays ordered."
                  onClick={() =>
                    void run(() =>
                      useSessionStore.getState().dispatchAction({
                        action: 'turn-control',
                        session_id: session.id,
                        command: controls.interrupt!,
                      }),
                    )
                  }
                  className={buttonClass}
                >
                  Interrupt
                </button>
              ) : (
                uncertain &&
                !session.active_prompt_id && (
                  <>
                    <button
                      type="button"
                      disabled={busy}
                      onClick={() =>
                        void run(() =>
                          useSessionStore.getState().dispatchAction({
                            action: 'turn-control',
                            session_id: session.id,
                            command: { type: 'resolve_steering', data: { steering_id: steering.command_id } },
                          }),
                        )
                      }
                      className={buttonClass}
                    >
                      Retry queued prompt
                    </button>
                    <button
                      type="button"
                      disabled={busy}
                      onClick={() =>
                        void run(() =>
                          useSessionStore.getState().dispatchAction({
                            action: 'remove-queued-prompt',
                            session_id: session.id,
                            queue_id: steering.queued_prompt_id,
                          }),
                        )
                      }
                      className={buttonClass}
                    >
                      Remove queued prompt
                    </button>
                  </>
                )
              )}
            </div>
          )}
        </>
      )}
      {error && (
        <p role="alert" className="mt-1 text-danger">
          {error}
        </p>
      )}
    </div>
  );
}

const buttonClass =
  'inline-flex min-h-8 items-center rounded-full border border-line bg-surface px-2.5 py-1 text-[11.5px] font-medium text-fg hover:bg-raised disabled:opacity-60';
