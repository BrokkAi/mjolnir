// The elicitation entry point mounted above the composer.
//
// - No pending requests: render nothing.
// - A dock is always visible while something waits.
// - Phones auto-open the full-height sheet once for each unseen request, and
//   "Peek transcript" collapses it back to the dock without losing drafts.
// - Desktops show the spacious inline card by default and can expand it into
//   the same modal sheet.

import { useEffect, useMemo, useRef, useState } from 'react';
import type { ViewerSession } from '../../api/types';
import { useMediaQuery } from '../../components/shell/useMediaQuery';
import { useUiStore } from '../../stores/useUiStore';
import { ElicitationDock } from './ElicitationDock';
import { ElicitationSheet } from './ElicitationSheet';
import { pruneElicitationDrafts } from './elicitationDrafts';

export interface ElicitationHostProps {
  session: ViewerSession;
}

export function ElicitationHost({ session }: ElicitationHostProps) {
  const isPhone = !useMediaQuery('(min-width: 640px)');
  const [expanded, setExpanded] = useState(false);
  const [activeId, setActiveId] = useState<string | undefined>(
    session.pending_elicitations?.[0]?.id,
  );
  const [answeredIds, setAnsweredIds] = useState<ReadonlySet<string>>(() => new Set());
  const seenIds = useRef(new Set<string>());

  const pending = useMemo(() => session.pending_elicitations ?? [], [session.pending_elicitations]);
  const pendingIds = pending.map(request => request.id);
  const pendingSignature = pendingIds.join('\u001f');

  // Retire optimistic answers once the stream publishes them.
  useEffect(() => {
    setAnsweredIds(current => {
      if (current.size === 0) return current;
      const next = new Set([...current].filter(id => pendingIds.includes(id)));
      return next.size === current.size ? current : next;
    });
  }, [pendingSignature]); // eslint-disable-line react-hooks/exhaustive-deps

  const visible = useMemo(
    () => pending.filter(request => !answeredIds.has(request.id)),
    [pending, answeredIds],
  );

  // Keep the active request pointing at something still pending.
  useEffect(() => {
    if (visible.length === 0) {
      if (activeId !== undefined) setActiveId(undefined);
      return;
    }
    if (!activeId || !visible.some(request => request.id === activeId)) {
      setActiveId(visible[0].id);
    }
  }, [activeId, visible]);

  // Auto-open once per unseen request on a phone viewport.
  useEffect(() => {
    if (pending.length === 0) return;
    let unseen = false;
    for (const request of pending) {
      if (!seenIds.current.has(request.id)) {
        seenIds.current.add(request.id);
        unseen = true;
      }
    }
    if (unseen && isPhone) setExpanded(true);
  }, [isPhone, pendingSignature]); // eslint-disable-line react-hooks/exhaustive-deps

  // A request that left the stream also loses its draft. A summary row omits
  // `pending_elicitations`; it must not delete drafts the detail row still owns.
  useEffect(() => {
    if ((session as { detail?: boolean }).detail === false) return;
    pruneElicitationDrafts(session.id, pendingIds);
  }, [pendingSignature, session.id]); // eslint-disable-line react-hooks/exhaustive-deps

  // Back / Escape collapse the sheet before leaving the conversation.
  useEffect(() => {
    if (!expanded) return;
    return useUiStore.getState().registerBackLayer(() => setExpanded(false));
  }, [expanded]);

  useEffect(() => {
    if (visible.length === 0) setExpanded(false);
  }, [visible.length]);

  const active = visible.find(request => request.id === activeId) ?? visible[0];
  if (!active || visible.length === 0) return null;

  const pendingIndex = visible.indexOf(active);
  const onSubmitted = () => {
    setAnsweredIds(current => new Set([...current, active.id]));
  };

  const requestKey = `${session.id}:${active.id}`;

  return (
    <div className="min-w-0">
      <ElicitationDock
        request={active}
        pendingIndex={pendingIndex}
        pendingTotal={visible.length}
        onAnswer={() => setExpanded(true)}
      />
      {expanded ? (
        <ElicitationSheet
          key={requestKey}
          session={session}
          request={active}
          pendingIndex={pendingIndex}
          pendingTotal={visible.length}
          variant="modal"
          onPeek={() => setExpanded(false)}
          onSubmitted={onSubmitted}
        />
      ) : !isPhone ? (
        <div className="mt-2 min-w-0">
          <ElicitationSheet
            key={requestKey}
            session={session}
            request={active}
            pendingIndex={pendingIndex}
            pendingTotal={visible.length}
            variant="inline"
            onPeek={() => setExpanded(false)}
            onExpand={() => setExpanded(true)}
            onSubmitted={onSubmitted}
          />
        </div>
      ) : null}
    </div>
  );
}
