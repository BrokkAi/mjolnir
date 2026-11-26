import { useState, type ReactNode } from 'react';
import { Ban, ListOrdered, SquareTerminal, X } from 'lucide-react';
import { apiFetch } from '../../api/client';
import type { ViewerSession } from '../../api/types';
import { useSessionStore } from '../../stores/useSessionStore';
import { useUiStore } from '../../stores/useUiStore';
import { describeError, formatSince } from './format';

/**
 * Non-intrusive strips between the transcript and the composer for work that is
 * already in flight: queued prompts, user shells, background tasks, and the
 * review the daemon is running. Each control acts on its own row and reports
 * its own failure there.
 */

export interface LiveActivityBarsProps {
  session: ViewerSession;
}

function SectionHeading({ icon, children }: { icon: ReactNode; children: ReactNode }) {
  return (
    <h3 className="flex items-center gap-1.5 text-[10.5px] font-medium uppercase tracking-wide text-muted">
      {icon}
      {children}
    </h3>
  );
}

export function LiveActivityBars({ session }: LiveActivityBarsProps) {
  const [pending, setPending] = useState<Record<string, true>>({});
  const [errors, setErrors] = useState<Record<string, string>>({});

  const run = async (key: string, work: () => Promise<unknown>) => {
    setPending(current => ({ ...current, [key]: true }));
    setErrors(current => {
      if (!(key in current)) return current;
      const { [key]: _gone, ...rest } = current;
      return rest;
    });
    try {
      await work();
    } catch (error) {
      setErrors(current => ({ ...current, [key]: describeError(error) }));
    } finally {
      setPending(current => {
        const { [key]: _done, ...rest } = current;
        return rest;
      });
    }
  };

  const shells = session.active_user_shells ?? [];
  const tasks = session.background_tasks ?? [];
  const queued = session.queued_prompts ?? [];
  const review = session.turn_review;

  const openReviewer = (childId: string) => {
    useUiStore.getState().navigate({
      name: 'conversation',
      sessionId: childId,
      subagentParentId: session.id,
      childKind: 'reviewer',
    });
  };

  if (shells.length === 0 && tasks.length === 0 && queued.length === 0 && !review) return null;

  return (
    <div className="border-b border-line bg-surface" data-testid="live-activity-bars">
      <div className="mx-auto w-full max-w-3xl divide-y divide-line/60 px-3">
        {review && (
          <section className="flex flex-wrap items-center gap-x-2 gap-y-1 py-1.5 text-[11.5px]">
            <SectionHeading icon={<ListOrdered className="size-3.5" aria-hidden />}>Review</SectionHeading>
            <span className="rounded-full border border-line bg-raised px-1.5 py-0.5 text-[10.5px] text-fg">
              {review.status}
            </span>
            {review.verdict && (
              <span className={review.verdict.kind === 'failed' ? 'text-danger' : 'text-fg'} title={review.verdict.text}>
                {review.verdict.kind || 'verdict'}: <span className="text-muted">{review.verdict.text}</span>
              </span>
            )}
            {review.roles?.map((role, index) => {
              const label = `${role.label}: ${role.state}`;
              return role.child_session_id ? (
                <button
                  key={`${role.label}:${index}`}
                  type="button"
                  onClick={() => openReviewer(role.child_session_id as string)}
                  className="rounded-full border border-line px-1.5 py-0.5 text-[10.5px] text-accent hover:bg-raised"
                >
                  {label}
                </button>
              ) : (
                <span key={`${role.label}:${index}`} className="text-[10.5px] text-muted">
                  {label}
                </span>
              );
            })}
            {review.child_session_id && (
              <button
                type="button"
                onClick={() => openReviewer(review.child_session_id as string)}
                className="rounded-full border border-accent/40 bg-accent/10 px-2 py-0.5 text-[10.5px] text-accent hover:bg-accent/20"
              >
                Open review conversation
              </button>
            )}
          </section>
        )}

        {queued.length > 0 && (
          <section className="py-1.5">
            <SectionHeading icon={<ListOrdered className="size-3.5" aria-hidden />}>
              Queued prompts ({queued.length})
            </SectionHeading>
            <ul className="mt-1 space-y-1">
              {queued.map((prompt, index) => {
                const key = `queue:${prompt.id}`;
                return (
                  <li key={prompt.id} className="rounded-md border border-line bg-bg/50 px-2 py-1">
                    <div className="flex items-start gap-2">
                      <span className="min-w-0 flex-1 whitespace-pre-wrap break-words text-[12px] leading-snug text-fg">
                        <span className="mr-1 text-muted">{index + 1}.</span>
                        {prompt.text}
                      </span>
                      <button
                        type="button"
                        aria-label={`Remove queued prompt ${index + 1}`}
                        disabled={Boolean(pending[key])}
                        onClick={() =>
                          void run(key, () =>
                            useSessionStore.getState().dispatchAction({
                              action: 'remove-queued-prompt',
                              session_id: session.id,
                              queue_id: prompt.id,
                            }),
                          )
                        }
                        className="grid size-7 shrink-0 place-items-center rounded border border-line text-muted hover:text-danger disabled:opacity-50"
                      >
                        <X className="size-3.5" aria-hidden />
                      </button>
                    </div>
                    {errors[key] && (
                      <p role="alert" className="mt-1 text-[11px] text-danger">
                        {errors[key]}
                      </p>
                    )}
                  </li>
                );
              })}
            </ul>
          </section>
        )}

        {shells.length > 0 && (
          <section className="py-1.5">
            <SectionHeading icon={<SquareTerminal className="size-3.5" aria-hidden />}>
              Shells ({shells.length})
            </SectionHeading>
            <ul className="mt-1 space-y-1">
              {shells.map(shell => {
                const key = `shell:${shell.id}`;
                const elapsed = formatSince(shell.started_at_ms);
                return (
                  <li key={shell.id} className="rounded-md border border-line bg-bg/50 px-2 py-1">
                    <div className="flex items-center gap-2">
                      <span className="min-w-0 flex-1 truncate font-mono text-[12px] text-fg">
                        <span className="mr-1 text-muted">$</span>
                        {shell.command}
                      </span>
                      {elapsed && <span className="shrink-0 text-[10.5px] tabular-nums text-muted">{elapsed}</span>}
                      <button
                        type="button"
                        aria-label={`Cancel shell: ${shell.command}`}
                        disabled={Boolean(pending[key])}
                        onClick={() =>
                          void run(key, () =>
                            useSessionStore.getState().dispatchAction({
                              action: 'cancel-shell',
                              session_id: session.id,
                              shell_command_id: shell.id,
                            }),
                          )
                        }
                        className="shrink-0 rounded border border-line px-2 py-0.5 text-[11px] text-danger hover:bg-raised disabled:opacity-50"
                      >
                        Cancel
                      </button>
                    </div>
                    {errors[key] && (
                      <p role="alert" className="mt-1 text-[11px] text-danger">
                        {errors[key]}
                      </p>
                    )}
                  </li>
                );
              })}
            </ul>
          </section>
        )}

        {tasks.length > 0 && (
          <section className="py-1.5">
            <SectionHeading icon={<Ban className="size-3.5" aria-hidden />}>
              Background tasks ({tasks.length})
            </SectionHeading>
            <ul className="mt-1 space-y-1">
              {tasks.map(task => {
                const key = `task:${task.id}`;
                const elapsed = formatSince(task.started_at_ms);
                return (
                  <li key={task.id} className="rounded-md border border-line bg-bg/50 px-2 py-1">
                    <div className="flex items-center gap-2">
                      <span className="min-w-0 flex-1 truncate font-mono text-[12px] text-fg">
                        <span className="mr-1 text-muted">$</span>
                        {task.command}
                      </span>
                      {elapsed && <span className="shrink-0 text-[10.5px] tabular-nums text-muted">{elapsed}</span>}
                      {task.can_stop ? (
                        <button
                          type="button"
                          aria-label={`Stop background task: ${task.command}`}
                          disabled={Boolean(pending[key])}
                          onClick={() =>
                            void run(key, () =>
                              apiFetch(
                                `/api/v1/sessions/${encodeURIComponent(session.id)}/background-tasks/stop`,
                                { method: 'POST', json: { background_task_id: task.id } },
                              ),
                            )
                          }
                          className="shrink-0 rounded border border-line px-2 py-0.5 text-[11px] text-danger hover:bg-raised disabled:opacity-50"
                        >
                          {pending[key] ? 'Stopping…' : 'Stop'}
                        </button>
                      ) : (
                        <span className="shrink-0 text-[10.5px] text-muted">Stop unavailable</span>
                      )}
                    </div>
                    {errors[key] && (
                      <p role="alert" className="mt-1 text-[11px] text-danger">
                        {errors[key]}
                      </p>
                    )}
                  </li>
                );
              })}
            </ul>
          </section>
        )}
      </div>
    </div>
  );
}
