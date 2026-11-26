import { useEffect, useState } from 'react';
import { SuspendedSessions } from './SuspendedSessions';
import { WikiPanel } from './WikiPanel';

/** Props the shell passes. Keep this shape when replacing the placeholder. */
export type ResumeViewProps = {
  workspaceId: string;
  sessionId?: string;
  wikiId?: string;
  from?: 'notifications';
};

type Tab = 'suspended' | 'wiki';

export default function ResumeView({ workspaceId, sessionId, wikiId }: ResumeViewProps) {
  const [tab, setTab] = useState<Tab>(wikiId ? 'wiki' : 'suspended');

  useEffect(() => {
    if (wikiId) setTab('wiki');
  }, [wikiId]);

  return (
    <section className="mx-auto flex w-full max-w-3xl flex-col gap-3 p-4">
      <div>
        <h1 className="text-lg font-semibold tracking-tight text-fg">Resume and SessionWiki</h1>
        <p className="mt-0.5 text-xs text-muted">
          Bring a stopped session back, or restore a conversation from Mjolnir&apos;s archive.
        </p>
      </div>
      <div
        role="tablist"
        aria-label="Resume sections"
        className="grid grid-cols-2 gap-1 rounded-lg border border-line bg-raised p-1"
      >
        <button
          type="button"
          role="tab"
          aria-selected={tab === 'suspended'}
          onClick={() => setTab('suspended')}
          className={`min-h-9 rounded-md px-3 text-sm font-medium ${
            tab === 'suspended' ? 'bg-accent text-accent-fg' : 'text-muted hover:text-fg'
          }`}
        >
          Suspended sessions
        </button>
        <button
          type="button"
          role="tab"
          aria-selected={tab === 'wiki'}
          onClick={() => setTab('wiki')}
          className={`min-h-9 rounded-md px-3 text-sm font-medium ${
            tab === 'wiki' ? 'bg-accent text-accent-fg' : 'text-muted hover:text-fg'
          }`}
        >
          Session Wiki
        </button>
      </div>
      {tab === 'suspended' ? (
        <SuspendedSessions workspaceId={workspaceId} sessionId={sessionId} />
      ) : (
        <WikiPanel workspaceId={workspaceId} wikiId={wikiId} />
      )}
    </section>
  );
}
