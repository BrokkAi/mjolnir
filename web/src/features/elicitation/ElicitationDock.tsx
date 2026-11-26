// The always-visible banner above the composer. It has to be readable at a
// glance and answerable in one tap, even when the sheet is collapsed.

import { MessageSquare, ShieldAlert } from 'lucide-react';
import type { ElicitationRequest } from '../../api/types';
import { previewText } from './elicitationLogic';

export interface ElicitationDockProps {
  request: ElicitationRequest;
  pendingIndex: number;
  pendingTotal: number;
  onAnswer: () => void;
}

export function ElicitationDock({
  request,
  pendingIndex,
  pendingTotal,
  onAnswer,
}: ElicitationDockProps) {
  const title = request.title || (request.tool_permission ? 'Tool permission' : 'Input needed');
  const preview = previewText(request.message);
  return (
    <section
      data-testid="elicitation-dock"
      aria-label="Question waiting"
      className="flex min-w-0 flex-wrap items-center gap-3 rounded-xl border border-accent/50 bg-raised p-3 shadow-sm"
    >
      <div className="grid size-9 shrink-0 place-items-center rounded-lg bg-accent/15 text-accent">
        {request.tool_permission ? (
          <ShieldAlert className="size-5" aria-hidden />
        ) : (
          <MessageSquare className="size-5" aria-hidden />
        )}
      </div>
      <div className="min-w-0 flex-1 basis-[12rem]">
        <div className="flex min-w-0 flex-wrap items-center gap-2">
          <span className="min-w-0 break-words text-sm font-semibold text-fg">{title}</span>
          <span className="shrink-0 rounded-full border border-accent/40 bg-accent/15 px-2 py-0.5 text-[11px] font-semibold uppercase tracking-wide text-accent">
            {pendingIndex + 1} of {pendingTotal}
          </span>
        </div>
        {preview && (
          <p className="mt-0.5 line-clamp-2 min-w-0 break-words text-[13px] leading-snug text-fg">
            {preview}
          </p>
        )}
      </div>
      <button
        type="button"
        onClick={onAnswer}
        className="min-h-11 shrink-0 rounded-lg bg-accent px-5 text-sm font-semibold text-accent-fg hover:opacity-90"
      >
        Answer
      </button>
    </section>
  );
}
