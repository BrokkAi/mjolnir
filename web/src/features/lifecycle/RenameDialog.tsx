import { LoaderCircle } from 'lucide-react';
import { useState } from 'react';
import type { ViewerSession } from '../../api/types';
import { useSessionStore } from '../../stores/useSessionStore';
import { useUiStore } from '../../stores/useUiStore';
import { DialogButton, ModalFrame } from './ModalFrame';

export function RenameDialog({
  session,
  onClose,
}: {
  session: ViewerSession;
  onClose: () => void;
}) {
  const dispatchAction = useSessionStore(state => state.dispatchAction);
  const pushToast = useUiStore(state => state.pushToast);
  const [title, setTitle] = useState(session.given_title ?? session.title);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const trimmed = title.trim();

  const submit = async () => {
    if (!trimmed || busy) return;
    setBusy(true);
    setError(null);
    try {
      await dispatchAction({ action: 'rename', session_id: session.id, title: trimmed });
      pushToast('success', 'Session renamed.');
      onClose();
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
    } finally {
      setBusy(false);
    }
  };

  return (
    <ModalFrame
      title="Rename session"
      subtitle={session.id}
      onClose={onClose}
      footer={
        <>
          <DialogButton onClick={onClose} disabled={busy}>
            Cancel
          </DialogButton>
          <DialogButton
            tone="primary"
            onClick={() => void submit()}
            disabled={!trimmed || busy}
          >
            {busy && <LoaderCircle className="size-4 animate-spin" aria-hidden />}
            Rename
          </DialogButton>
        </>
      }
    >
      <label className="flex flex-col gap-1.5 text-sm">
        <span className="text-xs font-medium uppercase tracking-wide text-muted">Session name</span>
        <input
          type="text"
          value={title}
          autoFocus
          onChange={event => setTitle(event.target.value)}
          onKeyDown={event => {
            if (event.key === 'Enter') {
              event.preventDefault();
              void submit();
            }
          }}
          className="h-11 rounded-lg border border-line bg-bg px-3 text-sm text-fg"
        />
      </label>
      {error && (
        <p role="alert" className="mt-3 rounded-lg bg-danger/10 p-2 text-sm text-danger">
          {error}
        </p>
      )}
    </ModalFrame>
  );
}
