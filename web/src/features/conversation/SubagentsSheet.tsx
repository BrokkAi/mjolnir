import { useEffect, useRef, useState } from 'react';
import {
  ChevronDown,
  ChevronRight,
  LoaderCircle,
  MessagesSquare,
  Users,
} from 'lucide-react';
import { getNativeAgentHistory, getToolDetail } from '../../api/client';
import type { NativeAgent, NativeHistoryItem, ToolCallDetailResponse } from '../../api/types';
import { useSessionStore } from '../../stores/useSessionStore';
import { useUiStore } from '../../stores/useUiStore';
import { OverlaySheet } from './OverlaySheet';
import { ToolDetailBody } from './ToolCard';
import { describeError } from './format';

/**
 * The sub-agent and reviewer switcher. Mjolnir children are real sessions and
 * open as conversations; native sub-agents live inside their parent's
 * transcript, so their history is read lazily in place.
 */

export interface SubagentsSheetProps {
  sessionId: string;
  subagentSessionIds?: string[];
  reviewerSessionIds?: string[];
  nativeAgents?: NativeAgent[];
  onClose: () => void;
}

function StateDot({ className }: { className: string }) {
  return <span className={`size-1.5 shrink-0 rounded-full ${className}`} aria-hidden />;
}

function nativeStateDot(state: string): string {
  switch (state) {
    case 'running':
      return 'bg-ok animate-pulse';
    case 'completed':
      return 'bg-ok';
    case 'failed':
      return 'bg-danger';
    case 'cancelled':
      return 'bg-warn';
    default:
      return 'bg-muted';
  }
}

function NativeHistoryRow({ item, ownerSessionId, nativeChildId }: { item: NativeHistoryItem; ownerSessionId: string; nativeChildId: string }) {
  const [detail, setDetail] = useState<ToolCallDetailResponse | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [open, setOpen] = useState(false);
  const alive = useRef(true);
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);

  const load = async () => {
    if (open) {
      setOpen(false);
      return;
    }
    setOpen(true);
    if (detail || loading) return;
    setLoading(true);
    setError(null);
    try {
      const response = await getToolDetail(ownerSessionId, item.stable_id, nativeChildId);
      if (alive.current) setDetail(response);
    } catch (failure) {
      if (alive.current) setError(describeError(failure));
    } finally {
      if (alive.current) setLoading(false);
    }
  };

  return (
    <div className="rounded-md border border-line/70 bg-bg/40">
      <button
        type="button"
        onClick={() => void load()}
        aria-expanded={open}
        className="flex min-h-8 w-full items-center gap-2 px-2 py-1 text-left"
      >
        <ChevronRight
          aria-hidden
          className={`size-3.5 shrink-0 text-muted transition-transform ${open ? 'rotate-90' : ''}`}
        />
        <span className="shrink-0 font-mono text-[10px] uppercase tracking-wide text-muted">{item.role}</span>
        <span className="min-w-0 flex-1 truncate text-[12px] text-fg">{item.text.split('\n')[0] || item.stable_id}</span>
        {item.tool_detail_trimmed && (
          <span className="shrink-0 rounded bg-raised px-1 py-0.5 text-[9.5px] text-muted">trimmed</span>
        )}
      </button>
      {open && (
        <div className="border-t border-line/70 px-2 py-1.5 [overflow-anchor:none]">
          {loading && (
            <p className="flex items-center gap-2 py-1 text-[12px] text-muted" role="status">
              <LoaderCircle className="size-3.5 animate-spin" aria-hidden />
              Loading…
            </p>
          )}
          {error && (
            <p role="alert" className="py-1 text-[12px] text-danger">
              {error}
            </p>
          )}
          {detail ? (
            <ToolDetailBody detail={detail} />
          ) : (
            !loading &&
            !error && (
              <pre className="overflow-x-auto whitespace-pre-wrap break-words font-mono text-[12px] leading-relaxed text-fg">
                {item.text}
              </pre>
            )
          )}
        </div>
      )}
    </div>
  );
}

function NativeAgentRow({ agent, ownerSessionId }: { agent: NativeAgent; ownerSessionId: string }) {
  const [expanded, setExpanded] = useState(false);
  const [items, setItems] = useState<NativeHistoryItem[]>([]);
  const [hasMore, setHasMore] = useState(false);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const alive = useRef(true);
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);

  const load = async (before?: NativeHistoryItem) => {
    setLoading(true);
    setError(null);
    try {
      const page = await getNativeAgentHistory(
        ownerSessionId,
        agent.session_id,
        before ? { beforePosition: before.position, beforeId: before.stable_id } : undefined,
      );
      if (!alive.current) return;
      setItems(current => (before ? [...page.items, ...current] : page.items));
      setHasMore(page.has_more);
    } catch (failure) {
      if (alive.current) setError(describeError(failure));
    } finally {
      if (alive.current) setLoading(false);
    }
  };

  const toggle = () => {
    if (!expanded && items.length === 0 && !loading) void load();
    setExpanded(value => !value);
  };

  return (
    <div className="overflow-hidden rounded-lg border border-line bg-surface">
      <button
        type="button"
        onClick={toggle}
        aria-expanded={expanded}
        className="flex min-h-10 w-full items-center gap-2 px-2.5 py-1.5 text-left"
      >
        <StateDot className={nativeStateDot(agent.state)} />
        <span className="min-w-0 flex-1 truncate text-[13px] font-medium text-fg">{agent.name || agent.session_id}</span>
        <span className="shrink-0 text-[11px] text-muted">{agent.state}</span>
        {agent.availability !== 'available' && (
          <span className="shrink-0 text-[10.5px] text-warn">
            {agent.availability_reason || agent.availability}
          </span>
        )}
        <ChevronDown
          aria-hidden
          className={`size-4 shrink-0 text-muted transition-transform ${expanded ? 'rotate-180' : ''}`}
        />
      </button>
      {expanded && (
        <div className="space-y-1.5 border-t border-line px-2.5 py-2 [overflow-anchor:none]">
          {loading && items.length === 0 && (
            <p className="flex items-center gap-2 text-[12px] text-muted" role="status">
              <LoaderCircle className="size-3.5 animate-spin" aria-hidden />
              Loading native agent history…
            </p>
          )}
          {error && (
            <div className="rounded border border-danger/40 bg-danger/10 p-2">
              <p role="alert" className="text-[12px] text-danger">
                {error}
              </p>
              <button
                type="button"
                onClick={() => void load()}
                className="mt-1 rounded border border-line bg-bg px-2 py-0.5 text-[11px] text-fg"
              >
                Try again
              </button>
            </div>
          )}
          {hasMore && (
            <button
              type="button"
              onClick={() => void load(items[0])}
              disabled={loading || items.length === 0}
              className="w-full rounded border border-line px-2 py-1 text-[11px] text-muted hover:text-fg disabled:opacity-60"
            >
              {loading ? 'Loading…' : 'Load earlier'}
            </button>
          )}
          {items.map(item => (
            <NativeHistoryRow
              key={item.stable_id}
              item={item}
              ownerSessionId={ownerSessionId}
              nativeChildId={agent.session_id}
            />
          ))}
          {!loading && !error && items.length === 0 && (
            <p className="py-1 text-[12px] text-muted">No history available yet.</p>
          )}
        </div>
      )}
    </div>
  );
}

export function SubagentsSheet({
  sessionId,
  subagentSessionIds = [],
  reviewerSessionIds = [],
  nativeAgents = [],
  onClose,
}: SubagentsSheetProps) {
  const rows = useSessionStore(state => state.sessionsById);
  const navigating = useRef(false);
  const total = subagentSessionIds.length + reviewerSessionIds.length + nativeAgents.length;

  const openChild = (childId: string, kind: 'subagent' | 'reviewer') => {
    navigating.current = true;
    useUiStore.getState().navigate({
      name: 'conversation',
      sessionId: childId,
      subagentParentId: sessionId,
      ...(kind === 'reviewer' ? { childKind: 'reviewer' as const } : {}),
    });
  };

  const childRow = (id: string) => {
    const row = rows[id];
    return {
      title: row?.title || id,
      meta: row ? `${row.lifecycle}${row.transitioning ? ' · transitioning' : ''}` : 'not loaded',
    };
  };

  return (
    <OverlaySheet
      title="Sub-agents & reviewers"
      subtitle={total > 0 ? `${total} linked` : undefined}
      onClose={onClose}
      navigatingRef={navigating}
      testId="subagents-sheet"
    >
      {total === 0 && <p className="py-6 text-center text-sm text-muted">No sub-agents or reviewers.</p>}

      {subagentSessionIds.length > 0 && (
        <section className="mb-4">
          <h3 className="mb-1 flex items-center gap-1.5 text-[11px] uppercase tracking-wide text-muted">
            <Users className="size-3.5" aria-hidden />
            Sub-agents ({subagentSessionIds.length})
          </h3>
          <ul className="divide-y divide-line/60 overflow-hidden rounded-lg border border-line">
            {subagentSessionIds.map(id => {
              const child = childRow(id);
              return (
                <li key={id}>
                  <button
                    type="button"
                    onClick={() => openChild(id, 'subagent')}
                    className="flex min-h-10 w-full items-center gap-2 px-2.5 py-1.5 text-left hover:bg-raised/60"
                  >
                    <span className="min-w-0 flex-1 truncate text-[13px] text-fg">{child.title}</span>
                    <span className="shrink-0 text-[11px] text-muted">{child.meta}</span>
                    <ChevronRight className="size-4 shrink-0 text-muted" aria-hidden />
                  </button>
                </li>
              );
            })}
          </ul>
        </section>
      )}

      {reviewerSessionIds.length > 0 && (
        <section className="mb-4">
          <h3 className="mb-1 flex items-center gap-1.5 text-[11px] uppercase tracking-wide text-muted">
            <MessagesSquare className="size-3.5" aria-hidden />
            Reviewers ({reviewerSessionIds.length})
          </h3>
          <ul className="divide-y divide-line/60 overflow-hidden rounded-lg border border-line">
            {reviewerSessionIds.map(id => {
              const child = childRow(id);
              return (
                <li key={id}>
                  <button
                    type="button"
                    onClick={() => openChild(id, 'reviewer')}
                    className="flex min-h-10 w-full items-center gap-2 px-2.5 py-1.5 text-left hover:bg-raised/60"
                  >
                    <span className="min-w-0 flex-1 truncate text-[13px] text-fg">{child.title}</span>
                    <span className="shrink-0 text-[11px] text-muted">{child.meta}</span>
                    <ChevronRight className="size-4 shrink-0 text-muted" aria-hidden />
                  </button>
                </li>
              );
            })}
          </ul>
        </section>
      )}

      {nativeAgents.length > 0 && (
        <section>
          <h3 className="mb-1 text-[11px] uppercase tracking-wide text-muted">
            Native sub-agents ({nativeAgents.length})
          </h3>
          <div className="space-y-1.5">
            {nativeAgents.map(agent => (
              <NativeAgentRow key={agent.session_id} agent={agent} ownerSessionId={sessionId} />
            ))}
          </div>
        </section>
      )}
    </OverlaySheet>
  );
}
