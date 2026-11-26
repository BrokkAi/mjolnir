import { useEffect, useState } from 'react';
import { AlertTriangle, ChevronDown, LoaderCircle, Terminal } from 'lucide-react';
import { getToolDetail } from '../../api/client';
import type {
  BrowserTranscriptEntry,
  ChatEntry,
  JsonValue,
  TerminalOutputRecord,
  ToolCallDetailResponse,
  TranscriptBody,
} from '../../api/types';
import { UnifiedDiffView } from './UnifiedDiffView';
import { describeError, formatDuration } from './format';

/**
 * One tool call, one line until tapped.
 *
 * The expanded body is never rendered before the tap and never while
 * collapsed, which keeps long-output rows cheap. A trimmed row fetches its
 * full output only while expanded and keeps a full response in the module
 * cache, keyed by the entry version, so collapsing and reopening an unchanged
 * card costs nothing while a later update to the same call fetches fresh
 * output. An unavailable response is never cached, so reopening retries.
 */

const detailCache = new Map<string, ToolCallDetailResponse>();

function cacheKey(sessionId: string, stableId: string, version: string): string {
  return `${sessionId}\u0000${stableId}\u0000${version}`;
}

/** The part of the cache key that changes when the transcript updates the call. */
export function toolDetailVersion(entry: Pick<BrowserTranscriptEntry, 'updated_seq' | 'tool_status'>): string {
  return `${entry.updated_seq}\u0000${entry.tool_status ?? ''}`;
}

type DetailResult =
  | { key: string; response: ToolCallDetailResponse }
  | { key: string; error: string };

export interface ToolDetailState {
  detail: ToolCallDetailResponse | undefined;
  loading: boolean;
  error: string | null;
  retry: () => void;
}

/**
 * Fetches the full detail of one tool call while `enabled`. Each enable, entry
 * version change, or retry runs one request; results for another key are never
 * shown.
 */
export function useToolDetail(
  sessionId: string,
  stableId: string | undefined,
  version: string,
  enabled: boolean,
): ToolDetailState {
  const key = stableId ? cacheKey(sessionId, stableId, version) : undefined;
  const [result, setResult] = useState<DetailResult | undefined>();
  const [attempt, setAttempt] = useState(0);

  useEffect(() => {
    if (!enabled || !key || !stableId) return;
    const cached = detailCache.get(key);
    if (cached) {
      setResult({ key, response: cached });
      return;
    }
    let cancelled = false;
    getToolDetail(sessionId, stableId).then(
      response => {
        if (response.status === 'full') detailCache.set(key, response);
        if (!cancelled) setResult({ key, response });
      },
      error => {
        if (!cancelled) setResult({ key, error: describeError(error) });
      },
    );
    return () => {
      cancelled = true;
    };
  }, [enabled, key, sessionId, stableId, attempt]);

  const current = result && result.key === key ? result : undefined;
  return {
    detail: current && 'response' in current ? current.response : undefined,
    loading: enabled && Boolean(key) && !current,
    error: current && 'error' in current ? current.error : null,
    retry: () => {
      setResult(undefined);
      setAttempt(count => count + 1);
    },
  };
}

/** Test hook: forget fetched details between cases. */
export function resetToolDetailCache(): void {
  detailCache.clear();
}

export interface ToolCardProps {
  entry: BrowserTranscriptEntry;
  sessionId: string;
  /** Runs just before the card expands, so follow is released first. */
  onBeforeExpand: () => void;
}

const STATUS_DOT: Record<string, string> = {
  running: 'bg-warn animate-pulse',
  done: 'bg-ok',
  failed: 'bg-danger',
};

function statusDot(status: string | undefined): string {
  return (status && STATUS_DOT[status]) || 'bg-muted';
}

function formatJson(value: unknown): string {
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return '';
  }
}

function bodyText(body: TranscriptBody): string {
  switch (body.kind) {
    case 'system':
      return body.text;
    case 'terminal_output':
      return body.record.output;
    default:
      return '';
  }
}

function DisclosureBlock({ summary, body }: { summary: string[]; body: string[] }) {
  return (
    <section className="space-y-1">
      {summary.length > 0 && (
        <p className="font-mono text-[11.5px] leading-relaxed text-muted">{summary.join('\n')}</p>
      )}
      {body.length > 0 && (
        <pre className="overflow-x-auto whitespace-pre rounded-md border border-line bg-bg/60 p-2 font-mono text-[12px] leading-[1.55] text-fg [overflow-anchor:none]">
          {body.join('\n')}
        </pre>
      )}
    </section>
  );
}

function ToolOutput({ record }: { record: TerminalOutputRecord }) {
  return (
    <div className="space-y-1">
      <div className="flex items-center gap-2 text-[11px] text-muted">
        <Terminal className="size-3" aria-hidden />
        <span className="font-mono">{record.terminal_id}</span>
        {typeof record.exit_code === 'number' && (
          <span className={record.exit_code === 0 ? 'text-ok' : 'text-danger'}>
            exit {record.exit_code}
          </span>
        )}
        {record.signal && <span className="text-warn">{record.signal}</span>}
        {record.truncated && <span className="text-warn">truncated</span>}
      </div>
      <pre className="max-h-96 overflow-auto whitespace-pre rounded-md border border-line bg-bg/60 p-2 font-mono text-[12px] leading-[1.55] text-fg [overflow-anchor:none]">
        {record.output}
      </pre>
    </div>
  );
}

function ChatEntryOutput({ entry }: { entry: ChatEntry }) {
  if (entry.tool_content.length === 0) return null;
  return (
    <pre className="max-h-96 overflow-auto whitespace-pre rounded-md border border-line bg-bg/60 p-2 font-mono text-[12px] leading-[1.55] text-fg [overflow-anchor:none]">
      {entry.tool_content.join('\n')}
    </pre>
  );
}

function hasContent(value: JsonValue | undefined): value is JsonValue {
  if (value === undefined || value === null) return false;
  if (typeof value === 'string') return value.length > 0;
  if (Array.isArray(value)) return value.length > 0;
  if (typeof value === 'object') return Object.keys(value).length > 0;
  return true;
}

function ToolInput({ value }: { value: JsonValue }) {
  return (
    <section className="space-y-1">
      <h4 className="text-[11px] font-medium uppercase tracking-wide text-muted">Input</h4>
      <pre className="overflow-x-auto overflow-y-hidden whitespace-pre rounded-md border border-line bg-bg/60 p-2 font-mono text-[12px] leading-[1.55] text-fg [overflow-anchor:none]">
        {typeof value === 'string' ? value : formatJson(value)}
      </pre>
    </section>
  );
}

/**
 * The full detail of one tool call. Shared by the transcript card, history
 * rows, and the native-agent detail viewer, which fetch the same shape.
 */
export function ToolDetailBody({ detail }: { detail: ToolCallDetailResponse }) {
  if (detail.status === 'unavailable') {
    const summary = detail.summary;
    const summaryText = summary ? bodyText(summary) || formatJson(summary) : '';
    return (
      <div className="space-y-1.5 rounded-md border border-warn/40 bg-warn/10 p-2">
        <p className="flex items-center gap-1.5 text-[12px] text-warn">
          <AlertTriangle className="size-3.5" aria-hidden />
          Full output is unavailable
        </p>
        <p className="text-[12px] text-muted">{detail.unavailable_reason}</p>
        {summaryText && (
          <pre className="overflow-x-auto whitespace-pre rounded border border-line bg-bg/60 p-2 font-mono text-[12px] text-fg">
            {summaryText}
          </pre>
        )}
      </div>
    );
  }

  const extraBody = bodyText(detail.body);
  const terminalOutputs = detail.terminal_outputs;
  const duplicateBody = terminalOutputs.some(record => record.output && record.output === extraBody);
  const input = hasContent(detail.entry.tool_input)
    ? detail.entry.tool_input
    : hasContent(detail.call)
      ? detail.call
      : undefined;

  return (
    <div className="space-y-2">
      {input !== undefined && <ToolInput value={input} />}
      {detail.entry.tool_error && (
        <pre className="overflow-x-auto whitespace-pre rounded-md border border-danger/40 bg-danger/10 p-2 font-mono text-[12px] text-danger [overflow-anchor:none]">
          {detail.entry.tool_error}
        </pre>
      )}
      <ChatEntryOutput entry={detail.entry} />
      {terminalOutputs.map((record, index) => (
        <ToolOutput key={`${record.terminal_id}:${index}`} record={record} />
      ))}
      {extraBody && !duplicateBody && (
        <pre className="max-h-96 overflow-auto whitespace-pre rounded-md border border-line bg-bg/60 p-2 font-mono text-[12px] leading-[1.55] text-fg [overflow-anchor:none]">
          {extraBody}
        </pre>
      )}
      {detail.diffs.length > 0 && <UnifiedDiffView patches={detail.diffs} />}
    </div>
  );
}

export function ToolCard({ entry, sessionId, onBeforeExpand }: ToolCardProps) {
  const [expanded, setExpanded] = useState(false);
  const stableId = entry.stable_id;
  const trimmed = Boolean(entry.tool_detail_trimmed);
  const { detail, loading, error: detailError, retry } = useToolDetail(
    sessionId,
    stableId,
    toolDetailVersion(entry),
    expanded && trimmed,
  );

  const onToggle = () => {
    if (expanded) {
      setExpanded(false);
      return;
    }
    onBeforeExpand();
    setExpanded(true);
  };

  const duration =
    entry.tool_ended_at_ms != null && entry.recorded_at_ms != null
      ? formatDuration(entry.tool_ended_at_ms - entry.recorded_at_ms)
      : null;
  const disclosures = entry.disclosures ?? [];

  return (
    <article className="rounded-lg border border-line bg-surface [overflow-anchor:none]" data-testid="tool-card">
      <button
        type="button"
        onClick={onToggle}
        aria-expanded={expanded}
        aria-label={`${entry.label || 'Tool call'}${expanded ? ', collapse' : ', expand'}`}
        className="flex min-h-11 w-full items-center gap-2 px-2.5 py-1.5 text-left hover:bg-raised/60"
      >
        <span
          role="img"
          aria-label={`Tool ${entry.tool_status ?? 'pending'}`}
          className={`size-2 shrink-0 rounded-full ${statusDot(entry.tool_status)}`}
        />
        <span className="shrink-0 font-mono text-[10.5px] uppercase tracking-wide text-muted">
          {entry.glyph || 'tool'}
        </span>
        <span className="min-w-0 flex-1 truncate font-mono text-[13px] text-fg">{entry.label}</span>
        {entry.diffstats && entry.diffstats.length > 0 && (
          <span className="flex shrink-0 items-center gap-1">
            {entry.diffstats.slice(0, 3).map(stat => (
              <span
                key={stat.path}
                title={stat.path}
                className="inline-flex items-center gap-1 rounded border border-line bg-bg px-1.5 py-0.5 font-mono text-[10.5px]"
              >
                <span className="max-w-32 truncate text-muted">{stat.path.split('/').pop() || stat.path}</span>
                <span className="text-ok">+{stat.insertions}</span>
                <span className="text-danger">-{stat.deletions}</span>
              </span>
            ))}
          </span>
        )}
        {duration && <span className="shrink-0 text-[11px] tabular-nums text-muted">{duration}</span>}
        {trimmed && !expanded && (
          <span className="shrink-0 rounded bg-raised px-1.5 py-0.5 text-[10px] text-muted">trimmed</span>
        )}
        <ChevronDown
          aria-hidden
          className={`size-4 shrink-0 text-muted transition-transform ${expanded ? 'rotate-180' : ''}`}
        />
      </button>
      {expanded && (
        <div className="space-y-2 border-t border-line px-2.5 py-2 text-[13px] [overflow-anchor:none]" data-testid="tool-card-body">
          {disclosures.length > 0 && (
            <div className="space-y-2">
              {disclosures.map((disclosure, index) => (
                <DisclosureBlock key={index} summary={disclosure.summary} body={disclosure.body} />
              ))}
            </div>
          )}
          {entry.lines.length > 0 && (
            <pre className="overflow-x-auto whitespace-pre rounded-md border border-line bg-bg/60 p-2 font-mono text-[12px] leading-[1.55] text-fg [overflow-anchor:none]">
              {entry.lines.join('\n')}
            </pre>
          )}
          {trimmed && stableId && loading && (
            <p className="flex items-center gap-2 text-xs text-muted" role="status">
              <LoaderCircle className="size-3.5 animate-spin" aria-hidden />
              Loading full tool output…
            </p>
          )}
          {trimmed && stableId && detailError && (
            <div className="rounded-md border border-danger/40 bg-danger/10 p-2">
              <p role="alert" className="text-[12px] text-danger">
                Could not load the full tool output: {detailError}
              </p>
              <button
                type="button"
                onClick={retry}
                className="mt-1 rounded border border-line bg-surface px-2 py-1 text-[12px] text-fg"
              >
                Try again
              </button>
            </div>
          )}
          {detail && <ToolDetailBody detail={detail} />}
        </div>
      )}
    </article>
  );
}
