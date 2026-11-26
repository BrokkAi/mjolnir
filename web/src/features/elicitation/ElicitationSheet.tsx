// The spacious elicitation surface.
//
// One scrollport owns the whole question body (`flex-1 min-h-0 overflow-y-auto
// overscroll-contain [overflow-anchor:none]`) and a flex-pinned footer owns the
// actions. Nothing here resizes the page, hides the transcript or mutates
// `body`: "Peek transcript" only unmounts this sheet and drafts are already
// saved for the next mount.

import { Check, ChevronLeft, ChevronRight, ChevronUp, Eye, ShieldAlert } from 'lucide-react';
import { useEffect, useId, useMemo, useState } from 'react';
import { createPortal } from 'react-dom';
import type { ElicitationRequest, ElicitationResponse, ViewerSession } from '../../api/types';
import { MarkdownContent } from '../../components/MarkdownContent';
import { useSessionStore } from '../../stores/useSessionStore';
import { CompanionField, ElicitationFieldControl } from './ElicitationField';
import {
  buildQuestions,
  firstInvalidStep,
  isAnswered,
  isCompanionVisible,
  mergeDraftValues,
  acceptResponse,
  validateQuestion,
  validateRequest,
  type FieldErrors,
} from './elicitationLogic';
import {
  clearElicitationDraft,
  loadElicitationDraft,
  saveElicitationDraft,
} from './elicitationDrafts';

export interface ElicitationSheetProps {
  session: ViewerSession;
  request: ElicitationRequest;
  /** Zero-based position in the pending queue. */
  pendingIndex: number;
  pendingTotal: number;
  variant: 'modal' | 'inline';
  /** Collapse back to the dock (phone) or inline card (desktop). */
  onPeek: () => void;
  /** Desktop inline card to the expanded modal. */
  onExpand?: () => void;
  /** The response was accepted by the daemon; hide this request optimistically. */
  onSubmitted: () => void;
}

function initialDraft(sessionId: string, request: ElicitationRequest) {
  const stored = loadElicitationDraft(sessionId, request.id);
  const questions = buildQuestions(request);
  const maxStep = Math.max(0, questions.length - 1);
  const step = Math.min(Math.max(0, stored?.step ?? 0), maxStep);
  return { values: mergeDraftValues(request, stored?.values), step };
}

function actionErrorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export function ElicitationSheet({
  session,
  request,
  pendingIndex,
  pendingTotal,
  variant,
  onPeek,
  onExpand,
  onSubmitted,
}: ElicitationSheetProps) {
  const dispatchAction = useSessionStore(state => state.dispatchAction);
  const titleId = useId();
  const [draft, setDraft] = useState(() => initialDraft(session.id, request));
  const [errors, setErrors] = useState<FieldErrors>({});
  const [actionError, setActionError] = useState<string | null>(null);
  const [statusMessage, setStatusMessage] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [sent, setSent] = useState(false);

  const questions = useMemo(() => buildQuestions(request), [request]);
  const current = questions[draft.step];
  const isLast = questions.length === 0 || draft.step >= questions.length - 1;
  const unanswered = questions.filter(question => !isAnswered(question, draft.values)).length;
  const offersSkip = request.tool_permission == null || request.tool_permission.skip;

  // Persist every keystroke: a peek, a route change or a reconnect remounts
  // this component and must find the same answer.
  useEffect(() => {
    const secretFieldIds = new Set(request.fields.filter(field => field.secret).map(field => field.id));
    saveElicitationDraft(session.id, request.id, draft, secretFieldIds);
  }, [draft, request.fields, request.id, session.id]);

  const setValue = (fieldId: string, value: string | boolean | string[] | undefined) => {
    setDraft(state => ({
      ...state,
      values: { ...state.values, [fieldId]: value === undefined ? '' : value },
    }));
    setErrors(state => {
      if (!(fieldId in state)) return state;
      const next = { ...state };
      delete next[fieldId];
      return next;
    });
    setStatusMessage(null);
  };

  const showQuestion = (index: number) => {
    setDraft(state => ({ ...state, step: Math.max(0, Math.min(index, questions.length - 1)) }));
    setErrors({});
    setStatusMessage(null);
  };

  const validateStep = (index: number): boolean => {
    const question = questions[index];
    if (!question) return true;
    const stepErrors = validateQuestion(question, draft.values);
    if (Object.keys(stepErrors).length) {
      setErrors(stepErrors);
      setStatusMessage('Answer the highlighted field before continuing.');
      return false;
    }
    setErrors({});
    setStatusMessage(null);
    return true;
  };

  const send = async (response: ElicitationResponse) => {
    if (busy || sent) return;
    setBusy(true);
    setActionError(null);
    setStatusMessage('Sending answer…');
    try {
      await dispatchAction({
        action: 'respond-elicitation',
        session_id: session.id,
        elicitation_id: request.id,
        response,
      });
      clearElicitationDraft(session.id, request.id);
      setSent(true);
      setStatusMessage('Answer sent.');
      onSubmitted();
    } catch (error) {
      setActionError(actionErrorText(error));
      setStatusMessage(null);
    } finally {
      setBusy(false);
    }
  };

  const submitAccept = async () => {
    const allErrors = validateRequest(request, draft.values);
    if (Object.keys(allErrors).length) {
      setErrors(allErrors);
      const step = firstInvalidStep(request, draft.values);
      if (step != null) setDraft(state => ({ ...state, step }));
      setStatusMessage('Answer the highlighted fields before submitting.');
      return;
    }
    await send(acceptResponse(request, draft.values));
  };

  const advance = () => {
    if (busy || sent) return;
    if (!validateStep(draft.step)) return;
    if (isLast) {
      void submitAccept();
      return;
    }
    showQuestion(draft.step + 1);
  };

  const goBack = () => {
    if (busy || sent || draft.step === 0) return;
    showQuestion(draft.step - 1);
  };

  const disabled = busy || sent;

  const header = (
    <header className="flex shrink-0 items-start gap-3 border-b border-line bg-surface px-4 pt-3 pb-3 pt-safe">
      <div className="min-w-0 flex-1">
        <div className="flex flex-wrap items-center gap-2">
          <span className="rounded-full border border-accent/40 bg-accent/15 px-2 py-0.5 text-[11px] font-semibold uppercase tracking-wide text-accent">
            {pendingIndex + 1} of {pendingTotal}
          </span>
          {request.tool_permission && (
            <span className="rounded-full border border-warn/50 bg-warn/10 px-2 py-0.5 text-[11px] font-semibold uppercase tracking-wide text-warn">
              Permission
            </span>
          )}
        </div>
        <h2 id={titleId} className="mt-1 break-words text-lg font-semibold leading-snug text-fg">
          {request.title || 'Input needed'}
        </h2>
      </div>
      {variant === 'modal' ? (
        <button
          type="button"
          onClick={onPeek}
          className="inline-flex min-h-11 shrink-0 items-center gap-1.5 rounded-lg border border-line bg-raised px-3 text-sm font-semibold text-fg hover:bg-surface"
        >
          <Eye className="size-4" aria-hidden />
          Peek transcript
        </button>
      ) : (
        <button
          type="button"
          onClick={onExpand}
          className="inline-flex min-h-11 shrink-0 items-center gap-1.5 rounded-lg border border-line bg-raised px-3 text-sm font-semibold text-fg hover:bg-surface"
        >
          <ChevronUp className="size-4" aria-hidden />
          Expand
        </button>
      )}
    </header>
  );

  const permissionBanner = request.tool_permission ? (
    <section
      aria-label="Tool permission request"
      className="rounded-xl border border-warn/50 bg-warn/10 p-3"
    >
      <div className="flex items-center gap-2 text-warn">
        <ShieldAlert className="size-4 shrink-0" aria-hidden />
        <span className="text-sm font-semibold text-fg">Tool permission requested</span>
      </div>
      <p className="mt-1 text-[13px] leading-relaxed text-muted">
        Approve runs this tool call. Skip refuses this one call and leaves the turn running. Deny stops
        the turn.
      </p>
      <div className="mt-2 flex flex-wrap gap-2">
        <button
          type="button"
          onClick={() => void submitAccept()}
          disabled={disabled}
          className="min-h-11 rounded-lg bg-accent px-4 text-sm font-semibold text-accent-fg disabled:opacity-50"
        >
          Approve
        </button>
        <button
          type="button"
          onClick={() => void send({ action: 'cancel' })}
          disabled={disabled}
          className="min-h-11 rounded-lg border border-danger/50 bg-danger/10 px-4 text-sm font-semibold text-danger disabled:opacity-50"
        >
          Deny
        </button>
        <button
          type="button"
          onClick={() => void send({ action: 'decline' })}
          disabled={disabled || !offersSkip}
          title={offersSkip ? undefined : 'This request offers no one-time rejection.'}
          className="min-h-11 rounded-lg border border-line bg-raised px-4 text-sm font-semibold text-fg disabled:opacity-50"
        >
          Skip
        </button>
      </div>
    </section>
  ) : null;

  const questionBody = current ? (
    <div className="min-w-0 space-y-3" data-testid="elicitation-question">
      <ElicitationFieldControl
        field={current.field}
        value={draft.values[current.field.id]}
        onChange={value => setValue(current.field.id, value)}
        error={errors[current.field.id]}
        disabled={disabled}
        customChoiceTitle={
          current.role === 'alternative'
            ? current.companion?.title || 'Other / Custom answer'
            : undefined
        }
        onShortcut={() => void submitAccept()}
      />
      {current.companion &&
        isCompanionVisible(current, draft.values) && (
          <CompanionField
            field={current.companion}
            value={draft.values[current.companion.id]}
            onChange={value => setValue(current.companion!.id, value)}
            error={errors[current.companion.id]}
            disabled={disabled}
            autoFocus={current.role !== 'alongside'}
            onShortcut={() => void submitAccept()}
          />
        )}
    </div>
  ) : (
    <p className="text-sm text-muted">This request has no fields. Approve it to continue.</p>
  );

  const body = (
    <div
      data-testid="elicitation-sheet-body"
      className="flex-1 min-h-0 overflow-y-auto overscroll-contain [overflow-anchor:none] px-4 py-4"
    >
      <div className="mx-auto flex w-full max-w-2xl min-w-0 flex-col gap-4">
        {permissionBanner}
        <MarkdownContent text={request.message} />
        {request.description && (
          <MarkdownContent text={request.description} className="text-[14px] text-muted [&_p]:text-muted" />
        )}
        {questions.length > 0 && (
          <div className="flex flex-wrap items-center gap-x-3 gap-y-1 border-t border-line pt-3 text-[13px]">
            <span className="font-semibold text-fg">
              Step {Math.min(draft.step + 1, questions.length)} of {questions.length}
            </span>
            {unanswered > 0 && (
              <span className="text-muted">
                {unanswered} unanswered
              </span>
            )}
          </div>
        )}
        {questionBody}
      </div>
    </div>
  );

  const message = actionError ? (
    <p role="alert" className="mb-2 break-words rounded-lg border border-danger/50 bg-danger/10 px-3 py-2 text-sm text-danger">
      {actionError}
    </p>
  ) : statusMessage ? (
    <p role="status" className="mb-2 text-sm text-muted">
      {statusMessage}
    </p>
  ) : null;

  const footer = (
    <footer className="shrink-0 border-t border-line bg-surface px-4 pt-3 pb-3 pb-safe pb-keyboard">
      <div className="mx-auto w-full max-w-2xl">
        {message}
        <div className="flex flex-wrap items-center gap-2">
          <button
            type="button"
            onClick={goBack}
            disabled={disabled || draft.step === 0 || questions.length === 0}
            className="inline-flex min-h-11 items-center gap-1 rounded-lg border border-line bg-raised px-3 text-sm font-semibold text-fg disabled:opacity-40"
          >
            <ChevronLeft className="size-4" aria-hidden />
            Back
          </button>
          {!isLast && (
            <button
              type="button"
              onClick={advance}
              disabled={disabled}
              className="inline-flex min-h-11 items-center gap-1 rounded-lg bg-accent px-4 text-sm font-semibold text-accent-fg disabled:opacity-50"
            >
              Next
              <ChevronRight className="size-4" aria-hidden />
            </button>
          )}
          {isLast && (
            <button
              type="button"
              onClick={() => void submitAccept()}
              disabled={disabled}
              className="inline-flex min-h-11 items-center gap-1 rounded-lg bg-accent px-4 text-sm font-semibold text-accent-fg disabled:opacity-50"
            >
              <Check className="size-4" aria-hidden />
              Submit
            </button>
          )}
          <div className="ms-auto flex flex-wrap items-center gap-2">
            <button
              type="button"
              onClick={() => void send({ action: 'decline' })}
              disabled={disabled || !offersSkip}
              title={offersSkip ? 'Refuse this request once' : 'This request offers no one-time rejection.'}
              className="min-h-11 rounded-lg border border-line bg-raised px-3 text-sm font-semibold text-fg disabled:opacity-40"
            >
              Decline
            </button>
            <button
              type="button"
              onClick={() => void send({ action: 'cancel' })}
              disabled={disabled}
              className="min-h-11 rounded-lg border border-line bg-raised px-3 text-sm font-semibold text-fg disabled:opacity-40"
            >
              Cancel
            </button>
          </div>
        </div>
      </div>
    </footer>
  );

  if (variant === 'inline') {
    return (
      <section
        aria-labelledby={titleId}
        className="flex flex-col overflow-hidden rounded-2xl border border-line bg-surface shadow-sm"
      >
        {header}
        {body}
        {footer}
      </section>
    );
  }

  return createPortal(
    <div className="fixed inset-0 z-50 flex items-end justify-center sm:items-center">
      <button
        type="button"
        aria-label="Close elicitation"
        onClick={onPeek}
        className="absolute inset-0 bg-black/60"
      />
      <section
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        className="relative flex h-[92dvh] max-h-[92dvh] w-full flex-col overflow-hidden rounded-t-2xl border border-line bg-surface shadow-2xl sm:h-auto sm:max-h-[90dvh] sm:w-[min(720px,94vw)] sm:rounded-2xl"
      >
        {header}
        {body}
        {footer}
      </section>
    </div>,
    document.body,
  );
}
