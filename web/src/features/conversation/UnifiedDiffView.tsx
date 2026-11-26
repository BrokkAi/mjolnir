import type { DiffPatch } from '../../api/types';

/**
 * A unified diff without wrapping and without reflow. Each line is its own
 * block so `+`/`-`/hunk colours apply per line; horizontal scrolling keeps
 * long paths and code readable on a phone. `overflow-anchor: none` keeps the
 * browser from growing the card under the reader's thumb while it measures.
 */

function lineClass(line: string): string {
  if (line.startsWith('@@')) return 'text-accent';
  if (line.startsWith('+++') || line.startsWith('---')) return 'text-muted';
  if (line.startsWith('diff ') || line.startsWith('index ') || line.startsWith('new file')) return 'text-muted';
  if (line.startsWith('+')) return 'text-ok';
  if (line.startsWith('-')) return 'text-danger';
  return 'text-fg';
}

function DiffLines({ text }: { text: string }) {
  const lines = text.split('\n');
  return (
    <pre className="overflow-x-auto whitespace-pre rounded-md border border-line bg-raised p-2 font-mono text-[12px] leading-[1.55] [overflow-anchor:none]">
      <code>
        {lines.map((line, index) => (
          <span key={index} className={`block ${lineClass(line)}`}>
            {line.length ? line : ' '}
          </span>
        ))}
      </code>
    </pre>
  );
}

function PatchBlock({ patch }: { patch: DiffPatch }) {
  return (
    <div className="space-y-1">
      <div className="flex flex-wrap items-center gap-2 text-[11px] text-muted">
        <span className="text-ok">+{patch.insertions}</span>
        <span className="text-danger">-{patch.deletions}</span>
        {patch.created && <span className="rounded bg-raised px-1.5 py-0.5">new file</span>}
        {patch.truncated && <span className="rounded bg-raised px-1.5 py-0.5 text-warn">truncated</span>}
      </div>
      <DiffLines text={patch.text} />
    </div>
  );
}

export interface UnifiedDiffViewProps {
  /** A single unified diff body. */
  text?: string;
  /** Structured patches from a tool detail response. */
  patches?: DiffPatch[];
  className?: string;
}

export function UnifiedDiffView({ text, patches, className = '' }: UnifiedDiffViewProps) {
  if (patches && patches.length > 0) {
    return (
      <div className={`space-y-3 ${className}`} data-testid="unified-diff">
        {patches.map((patch, index) => (
          <PatchBlock key={index} patch={patch} />
        ))}
      </div>
    );
  }
  if (!text) return null;
  return (
    <div className={className} data-testid="unified-diff">
      <DiffLines text={text} />
    </div>
  );
}
