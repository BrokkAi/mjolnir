import { Check, LoaderCircle } from 'lucide-react';
import { useState } from 'react';
import type { ViewerSession, ViewerWorkspace } from '../../api/types';
import { useSessionStore } from '../../stores/useSessionStore';
import { useUiStore } from '../../stores/useUiStore';
import { DialogButton, ModalFrame } from './ModalFrame';

export function WorkspacePickerDialog({
  session,
  workspaces,
  onClose,
}: {
  session: ViewerSession;
  workspaces: ViewerWorkspace[];
  onClose: () => void;
}) {
  const dispatchAction = useSessionStore(state => state.dispatchAction);
  const pushToast = useUiStore(state => state.pushToast);
  const [selected, setSelected] = useState(session.workspace_id ?? workspaces[0]?.id ?? '');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const submit = async () => {
    if (!selected || busy) return;
    setBusy(true);
    setError(null);
    try {
      await dispatchAction({
        action: 'change-workspace',
        session_id: session.id,
        workspace_id: selected,
      });
      pushToast('success', 'Session moved to another workspace.');
      onClose();
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
    } finally {
      setBusy(false);
    }
  };

  return (
    <ModalFrame
      title="Change workspace"
      subtitle={session.title}
      onClose={onClose}
      footer={
        <>
          <DialogButton onClick={onClose} disabled={busy}>
            Cancel
          </DialogButton>
          <DialogButton
            tone="primary"
            onClick={() => void submit()}
            disabled={!selected || selected === session.workspace_id || busy}
          >
            {busy && <LoaderCircle className="size-4 animate-spin" aria-hidden />}
            Move session
          </DialogButton>
        </>
      }
    >
      {workspaces.length === 0 ? (
        <p className="text-sm text-muted">No workspaces are available.</p>
      ) : (
        <div role="radiogroup" aria-label="Workspace" className="flex flex-col gap-1.5">
          {workspaces.map(workspace => {
            const current = workspace.id === session.workspace_id;
            const checked = selected === workspace.id;
            return (
              <button
                key={workspace.id}
                type="button"
                role="radio"
                aria-checked={checked}
                onClick={() => setSelected(workspace.id)}
                className={`flex items-center justify-between gap-3 rounded-lg border p-3 text-left ${
                  checked ? 'border-accent bg-accent/10' : 'border-line hover:bg-raised'
                }`}
              >
                <span className="min-w-0">
                  <span className="block truncate text-sm font-medium text-fg">{workspace.name}</span>
                  <span className="block truncate font-mono text-xs text-muted">{workspace.id}</span>
                </span>
                <span className="flex shrink-0 items-center gap-1.5 text-xs text-muted">
                  {current && <span>Current</span>}
                  {checked && <Check className="size-4 text-accent" aria-hidden />}
                </span>
              </button>
            );
          })}
        </div>
      )}
      {error && (
        <p role="alert" className="mt-3 rounded-lg bg-danger/10 p-2 text-sm text-danger">
          {error}
        </p>
      )}
    </ModalFrame>
  );
}
