import { memo, useState, type Ref } from 'react';
import {
  AlertTriangle,
  Brain,
  ChevronDown,
  ListChecks,
  LoaderCircle,
} from 'lucide-react';
import { MarkdownContent } from '../../components/MarkdownContent';
import type { BrowserTranscriptEntry, TranscriptDisclosure } from '../../api/types';
import { ToolCard } from './ToolCard';
import { useThrottledValue } from './useThrottledValue';
import { STREAM_MARKDOWN_INTERVAL_MS } from './useConversationScroll';
import { formatClock } from './format';

/**
 * One transcript row. The parent keys rows by the entry's durable identity and
 * this component's custom comparator stops every other row from re-rendering
 * while the newest reply streams.
 */

export interface TranscriptRowProps {
  entry: BrowserTranscriptEntry;
  sessionId: string;
  /** Only true for the assistant reply currently being written. */
  isStreaming: boolean;
  /** Runs before expanding a disclosure, so end-follow is released first. */
  onBeforeExpand: () => void;
  /** Attached to the streaming reply (never to any other row). */
  streamingRef?: Ref<HTMLDivElement>;
}

const ROLE_LABEL: Record<string, string> = {
  user: 'You',
  agent: 'Assistant',
  message: 'Message',
  thought: 'Thinking',
  plan: 'Plan',
  'plan-proposal': 'Plan proposal',
  system: 'System',
  tool: 'Tool',
};

function entryText(entry: BrowserTranscriptEntry): string {
  return entry.lines.join('\n');
}

function roleLabel(entry: BrowserTranscriptEntry): string {
  return entry.label || ROLE_LABEL[entry.role] || entry.role;
}

function isErrorTone(tone: string): boolean {
  return tone === 'failed' || tone === 'error' || tone === 'danger';
}

/** The newest reply re-parses Markdown at most ten times a second. */
function StreamingMarkdown({ text }: { text: string }) {
  const throttled = useThrottledValue(text, STREAM_MARKDOWN_INTERVAL_MS);
  return <MarkdownContent text={throttled} />;
}

function Clock({ ms }: { ms: number | null }) {
  const text = formatClock(ms);
  if (!text) return null;
  return (
    <time className="shrink-0 tabular-nums" dateTime={new Date(ms as number).toISOString()}>
      {text}
    </time>
  );
}

function Disclosure({ disclosure, onBeforeExpand }: { disclosure: TranscriptDisclosure; onBeforeExpand: () => void }) {
  const [open, setOpen] = useState(false);
  const heading = disclosure.summary[0] ?? 'Details';
  return (
    <div className="overflow-hidden rounded-md border border-line/70 bg-bg/40">
      <button
        type="button"
        aria-expanded={open}
        onClick={() => {
          if (!open) onBeforeExpand();
          setOpen(!open);
        }}
        className="flex min-h-8 w-full items-center gap-2 px-2 py-1 text-left text-[12px] text-muted hover:text-fg"
      >
        <ChevronDown
          aria-hidden
          className={`size-3.5 shrink-0 transition-transform ${open ? 'rotate-180' : ''}`}
        />
        <span className="min-w-0 flex-1 truncate font-mono">{heading}</span>
        {disclosure.summary.length > 1 && (
          <span className="shrink-0 text-[10px] text-muted">{disclosure.summary.length} lines</span>
        )}
      </button>
      {open && disclosure.body.length > 0 && (
        <pre className="overflow-x-auto whitespace-pre border-t border-line/70 p-2 font-mono text-[12px] leading-[1.55] text-fg [overflow-anchor:none]">
          {disclosure.body.join('\n')}
        </pre>
      )}
    </div>
  );
}

function Disclosures({
  disclosures,
  onBeforeExpand,
}: {
  disclosures: TranscriptDisclosure[];
  onBeforeExpand: () => void;
}) {
  if (disclosures.length === 0) return null;
  return (
    <div className="mt-1.5 space-y-1">
      {disclosures.map((disclosure, index) => (
        <Disclosure key={index} disclosure={disclosure} onBeforeExpand={onBeforeExpand} />
      ))}
    </div>
  );
}

function AssistantRow({
  entry,
  isStreaming,
  onBeforeExpand,
  rowRef,
}: {
  entry: BrowserTranscriptEntry;
  isStreaming: boolean;
  onBeforeExpand: () => void;
  rowRef?: Ref<HTMLDivElement>;
}) {
  const text = entryText(entry);
  return (
    <article ref={rowRef} className="py-1" data-streaming={isStreaming ? 'true' : undefined}>
      <header className="mb-1 flex items-center gap-2 text-[11px] text-muted">
        <span aria-hidden className="text-accent">
          {entry.glyph || '•'}
        </span>
        <span className="font-medium">{roleLabel(entry)}</span>
        {isStreaming && (
          <LoaderCircle className="size-3 animate-spin text-accent" aria-label="Streaming" />
        )}
        <span className="ml-auto" />
        <Clock ms={entry.recorded_at_ms} />
      </header>
      {isStreaming ? <StreamingMarkdown text={text} /> : <MarkdownContent text={text} />}
      <Disclosures disclosures={entry.disclosures ?? []} onBeforeExpand={onBeforeExpand} />
    </article>
  );
}

function UserRow({ entry, rowRef }: { entry: BrowserTranscriptEntry; rowRef?: Ref<HTMLDivElement> }) {
  return (
    <div ref={rowRef} className="flex justify-end py-1">
      <article className="max-w-[85%] min-w-0 rounded-2xl rounded-br-sm border border-line border-r-2 border-r-accent/70 bg-raised px-3 py-2">
        <header className="mb-0.5 flex items-center justify-end gap-2 text-[10.5px] text-muted">
          <span className="font-medium">{entry.generated ? 'Mjolnir' : roleLabel(entry)}</span>
          <Clock ms={entry.recorded_at_ms} />
        </header>
        <MarkdownContent text={entryText(entry)} compact />
      </article>
    </div>
  );
}

function ThinkingRow({
  entry,
  onBeforeExpand,
  rowRef,
}: {
  entry: BrowserTranscriptEntry;
  onBeforeExpand: () => void;
  rowRef?: Ref<HTMLDivElement>;
}) {
  const [open, setOpen] = useState(false);
  const preview = entry.lines[0] ?? '';
  return (
    <article ref={rowRef} className="overflow-hidden rounded-lg border border-line/60 bg-surface/60">
      <button
        type="button"
        aria-expanded={open}
        onClick={() => {
          if (!open) onBeforeExpand();
          setOpen(!open);
        }}
        className="flex min-h-9 w-full items-center gap-2 px-2.5 py-1 text-left text-[12px] text-muted hover:text-fg"
      >
        <Brain className="size-3.5 shrink-0" aria-hidden />
        <span className="shrink-0 font-medium">{roleLabel(entry)}</span>
        {!open && <span className="min-w-0 flex-1 truncate opacity-80">{preview}</span>}
        {open && <span className="flex-1" />}
        <ChevronDown
          aria-hidden
          className={`size-3.5 shrink-0 transition-transform ${open ? 'rotate-180' : ''}`}
        />
      </button>
      {open && (
        <div className="border-t border-line/60 px-3 py-2">
          <MarkdownContent text={entryText(entry)} compact className="text-muted" />
        </div>
      )}
    </article>
  );
}

function SystemRow({ entry, rowRef }: { entry: BrowserTranscriptEntry; rowRef?: Ref<HTMLDivElement> }) {
  return (
    <article ref={rowRef} className="px-1 py-0.5 text-center text-[12px] text-muted">
      <span aria-hidden className="mr-1">
        {entry.glyph || '—'}
      </span>
      <span className="whitespace-pre-wrap break-words">{entryText(entry)}</span>
      <Clock ms={entry.recorded_at_ms} />
    </article>
  );
}

function ErrorRow({
  entry,
  onBeforeExpand,
  rowRef,
}: {
  entry: BrowserTranscriptEntry;
  onBeforeExpand: () => void;
  rowRef?: Ref<HTMLDivElement>;
}) {
  return (
    <article ref={rowRef} className="rounded-lg border border-danger/40 bg-danger/10 px-3 py-2">
      <header className="mb-1 flex items-center gap-2 text-[11px] text-danger">
        <AlertTriangle className="size-3.5" aria-hidden />
        <span className="font-medium">{roleLabel(entry)}</span>
        <Clock ms={entry.recorded_at_ms} />
      </header>
      <div className="whitespace-pre-wrap break-words text-[13px] text-fg">{entryText(entry)}</div>
      <Disclosures disclosures={entry.disclosures ?? []} onBeforeExpand={onBeforeExpand} />
    </article>
  );
}

function PlanRow({
  entry,
  onBeforeExpand,
  rowRef,
}: {
  entry: BrowserTranscriptEntry;
  onBeforeExpand: () => void;
  rowRef?: Ref<HTMLDivElement>;
}) {
  return (
    <article ref={rowRef} className="rounded-lg border border-accent/30 bg-accent/5 px-3 py-2">
      <header className="mb-1 flex items-center gap-2 text-[11px] text-accent">
        <ListChecks className="size-3.5" aria-hidden />
        <span className="font-medium">{roleLabel(entry)}</span>
        <Clock ms={entry.recorded_at_ms} />
      </header>
      <MarkdownContent text={entryText(entry)} compact />
      <Disclosures disclosures={entry.disclosures ?? []} onBeforeExpand={onBeforeExpand} />
    </article>
  );
}

function TranscriptRowImpl({
  entry,
  sessionId,
  isStreaming,
  onBeforeExpand,
  streamingRef,
}: TranscriptRowProps) {
  if (entry.role === 'tool') {
    return <ToolCard entry={entry} sessionId={sessionId} onBeforeExpand={onBeforeExpand} />;
  }
  if (entry.role === 'user') {
    return <UserRow entry={entry} rowRef={isStreaming ? streamingRef : undefined} />;
  }
  if (entry.role === 'thought') {
    return (
      <ThinkingRow entry={entry} onBeforeExpand={onBeforeExpand} rowRef={isStreaming ? streamingRef : undefined} />
    );
  }
  if (entry.role === 'system') {
    return isErrorTone(entry.tone) ? (
      <ErrorRow entry={entry} onBeforeExpand={onBeforeExpand} rowRef={isStreaming ? streamingRef : undefined} />
    ) : (
      <SystemRow entry={entry} rowRef={isStreaming ? streamingRef : undefined} />
    );
  }
  if (entry.role === 'plan' || entry.role === 'plan-proposal') {
    return (
      <PlanRow entry={entry} onBeforeExpand={onBeforeExpand} rowRef={isStreaming ? streamingRef : undefined} />
    );
  }
  if (isErrorTone(entry.tone)) {
    return (
      <ErrorRow entry={entry} onBeforeExpand={onBeforeExpand} rowRef={isStreaming ? streamingRef : undefined} />
    );
  }
  return (
    <AssistantRow
      entry={entry}
      isStreaming={isStreaming}
      onBeforeExpand={onBeforeExpand}
      rowRef={isStreaming ? streamingRef : undefined}
    />
  );
}

/**
 * Streaming revises the same entry object's `updated_seq`; everything else
 * keeps both its object identity and its sequence. Comparing sequence rather
 * than identity also covers a cache merge that rebuilt an equal row.
 */
export function sameTranscriptRowProps(prev: TranscriptRowProps, next: TranscriptRowProps): boolean {
  if (prev.sessionId !== next.sessionId) return false;
  if (prev.isStreaming !== next.isStreaming) return false;
  if (prev.onBeforeExpand !== next.onBeforeExpand) return false;
  if (prev.streamingRef !== next.streamingRef) return false;
  const a = prev.entry;
  const b = next.entry;
  if (a === b) return true;
  return a.id === b.id && a.stable_id === b.stable_id && a.updated_seq === b.updated_seq;
}

export const TranscriptRow = memo(TranscriptRowImpl, sameTranscriptRowProps);
TranscriptRow.displayName = 'TranscriptRow';
