import { AlertTriangle, LoaderCircle } from 'lucide-react';
import { useState } from 'react';
import { destroySession } from '../../api/client';
import type { ViewerSession } from '../../api/types';
import { useUiStore } from '../../stores/useUiStore';
import { DialogButton, ModalFrame } from './ModalFrame';

export function DestroyDialog({
  session,
  onClose,
}: {
  session: ViewerSession;
  onClose: () => void;
}) {
  const navigate = useUiStore(state => state.navigate);
  const pushToast = useUiStore(state => state.pushToast);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const destroy = async () => {
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      // `ControllerAction::Destroy` is deliberately absent from the wire;
      // destroy has its own POST route.
      await destroySession(session.id);
      const route = useUiStore.getState().route;
      if (route.name === 'conversation' && route.sessionId === session.id) {
        navigate({ name: 'workspace', workspaceId: session.workspace_id });
      }
      pushToast('success', 'Session destroyed.');
      onClose();
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
      setBusy(false);
    }
  };

  return (
    <ModalFrame
      title="Destroy session"
      subtitle={session.title}
      onClose={onClose}
      footer={
        <>
          <DialogButton onClick={onClose} disabled={busy}>
            Cancel
          </DialogButton>
          <DialogButton tone="danger" onClick={() => void destroy()} disabled={busy}>
            {busy && <LoaderCircle className="size-4 animate-spin" aria-hidden />}
            Destroy session
          </DialogButton>
        </>
      }
    >
      <div className="flex flex-col gap-3">
        <p className="flex items-start gap-2 rounded-lg bg-danger/10 p-3 text-sm text-danger">
          <AlertTriangle className="mt-0.5 size-4 shrink-0" aria-hidden />
          <span>
            Permanently remove this session, its environment, and its recovery archive. Work held
            only in the environment will be lost. This also destroys its sub-agents.
          </span>
        </p>
        <p className="text-sm text-muted">
          If environment cleanup fails, the session is still removed and a warning reports any
          resources that may remain. This cannot be undone.
        </p>
        {session.has_checkpoint && (
          <p className="text-sm text-muted">
            The existing checkpoint is deleted with the session, so it cannot be used to resume.
          </p>
        )}
        <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-xs">
          <dt className="text-muted">Session</dt>
          <dd className="font-mono text-fg">{session.id}</dd>
          <dt className="text-muted">Location</dt>
          <dd className="truncate text-fg">{session.display_location}</dd>
        </dl>
        {error && (
          <p role="alert" className="rounded-lg bg-danger/10 p-2 text-sm text-danger">
            {error}
          </p>
        )}
      </div>
    </ModalFrame>
  );
}
