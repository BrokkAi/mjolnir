import { useEffect } from 'react';
import { LoaderCircle } from 'lucide-react';
import type { SessionRow, ViewerSession, ViewerTarget, ViewerWorkspace } from '../../api/types';
import { useSessionStore } from '../../stores/useSessionStore';
import { useUiStore } from '../../stores/useUiStore';
import { ContainerSettingsDialog } from './ContainerSettingsDialog';
import { DestroyDialog } from './DestroyDialog';
import { DialogButton, ModalFrame } from './ModalFrame';
import { RenameDialog } from './RenameDialog';
import { SuspendDialog } from './SuspendDialog';
import { useLifecycleModal, type SessionModalAction } from './useLifecycleModal';
import { WorkspacePickerDialog } from './WorkspacePickerDialog';

const EMPTY_WORKSPACES: ViewerWorkspace[] = [];
const EMPTY_TARGETS: ViewerTarget[] = [];

const TITLES: Record<SessionModalAction['kind'], string> = {
  rename: 'Rename session',
  'change-workspace': 'Change workspace',
  suspend: 'Suspend session',
  'container-settings': 'Container settings',
  destroy: 'Destroy session',
};

/**
 * The single lifecycle dialog host. The conversation header and other views
 * only open a modal through `useLifecycleModal`; this component owns the
 * back-layer registration, detail fetch, and which dialog renders.
 */
export function SessionActionModal() {
  const modal = useLifecycleModal(state => state.modal);
  const closeModal = useLifecycleModal(state => state.closeModal);
  const row: SessionRow | undefined = useSessionStore(state =>
    modal ? state.sessionsById[modal.sessionId] : undefined,
  );
  const detailLoading = useSessionStore(state =>
    modal ? state.detailLoadingIds[modal.sessionId] === true : false,
  );
  const detailError = useSessionStore(state =>
    modal ? state.detailErrors[modal.sessionId] : undefined,
  );
  const meta = useSessionStore(state => state.meta);
  const workspaces = meta?.workspaces ?? EMPTY_WORKSPACES;
  const targets = meta?.targets ?? EMPTY_TARGETS;

  useEffect(() => {
    if (!modal) return;
    return useUiStore.getState().registerBackLayer(closeModal);
  }, [modal, closeModal]);

  useEffect(() => {
    if (!modal) return;
    void useSessionStore
      .getState()
      .ensureSessionDetail(modal.sessionId)
      .catch(() => {
        // The store keeps the message in `detailErrors`; the dialog shows it.
      });
  }, [modal]);

  if (!modal) return null;

  const session: ViewerSession | undefined =
    row && row.detail !== false ? row : undefined;

  if (!session) {
    return (
      <ModalFrame
        title={TITLES[modal.kind]}
        subtitle={modal.sessionId}
        onClose={closeModal}
        footer={<DialogButton onClick={closeModal}>Close</DialogButton>}
      >
        {detailError ? (
          <p role="alert" className="rounded-lg bg-danger/10 p-2.5 text-sm text-danger">
            Could not load this session: {detailError}
          </p>
        ) : row === undefined && !detailLoading ? (
          <p className="text-sm text-muted">
            This session is no longer available, so the action cannot be prepared.
          </p>
        ) : (
          <p role="status" className="flex items-center gap-2 text-sm text-muted">
            <LoaderCircle className="size-4 animate-spin" aria-hidden /> Loading session…
          </p>
        )}
      </ModalFrame>
    );
  }

  switch (modal.kind) {
    case 'rename':
      return <RenameDialog session={session} onClose={closeModal} />;
    case 'change-workspace':
      return (
        <WorkspacePickerDialog
          session={session}
          workspaces={workspaces}
          onClose={closeModal}
        />
      );
    case 'suspend':
      return <SuspendDialog session={session} onClose={closeModal} />;
    case 'container-settings':
      return (
        <ContainerSettingsDialog
          session={session}
          target={targets.find(target => target.id === session.target_id)}
          onClose={closeModal}
        />
      );
    case 'destroy':
      return <DestroyDialog session={session} onClose={closeModal} />;
  }
}
