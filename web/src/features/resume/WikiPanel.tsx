import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { ArrowLeft, LoaderCircle, RefreshCw, Search } from 'lucide-react';
import {
  getWikiBrief,
  getWikiHits,
  getWikiSession,
  restoreWikiSession,
  searchWiki,
  startWikiSync,
  watchWikiSync,
} from '../../api/client';
import type {
  WikiBrief,
  WikiHitBlock,
  WikiHitTranscript,
  WikiIndexState,
  WikiRestoreBody,
  WikiRow,
  WikiSessionInfo,
  WikiSyncStatus,
} from '../../api/types';
import { MarkdownContent } from '../../components/MarkdownContent';
import { useSessionStore } from '../../stores/useSessionStore';
import { useUiStore } from '../../stores/useUiStore';

export interface WikiPanelProps {
  workspaceId: string;
  wikiId?: string;
}

function describe(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function wikiStatusNote(status: WikiIndexState | undefined): string {
  switch (status) {
    case 'indexing':
      return 'The index is updating; results may be incomplete.';
    case 'version_mismatch':
      return 'This index was written by an older daemon. Run Sync to rebuild it.';
    default:
      return '';
  }
}

function syncProgress(status: WikiSyncStatus): string {
  const running = status.running;
  if (!running) {
    if (status.outcome === 'succeeded') return 'Sync finished.';
    if (status.outcome && typeof status.outcome === 'object') return `Sync failed: ${status.outcome.failed}`;
    return 'Sync started.';
  }
  const count = running.total === null ? `${running.done}` : `${running.done} / ${running.total}`;
  return `${running.phase} · ${running.tool} · ${count} (${running.adapter}/${running.adapters})`;
}

function Highlighted({ text, hits }: { text: string; hits: [number, number][] }) {
  const parts: ReactNode[] = [];
  let cursor = 0;
  for (const [start, end] of hits) {
    const from = Math.max(cursor, Math.min(start, text.length));
    const to = Math.max(from, Math.min(end, text.length));
    if (from > cursor) parts.push(text.slice(cursor, from));
    if (to > from) {
      parts.push(
        <mark key={`${from}-${to}`} className="rounded bg-warn/30 px-0.5 text-fg">
          {text.slice(from, to)}
        </mark>,
      );
    }
    cursor = Math.max(cursor, to);
  }
  if (cursor < text.length) parts.push(text.slice(cursor));
  return <>{parts}</>;
}

function HitBlockView({ block }: { block: WikiHitBlock }) {
  return (
    <div className="flex flex-col gap-1 rounded-lg border border-line bg-raised p-3">
      <p className="text-[11px] font-semibold uppercase tracking-wide text-muted">{block.role}</p>
      {block.omitted_before > 0 && (
        <p className="text-xs text-muted">… {block.omitted_before} earlier messages omitted</p>
      )}
      <pre className="whitespace-pre-wrap break-words font-mono text-xs leading-relaxed text-fg">
        <Highlighted text={block.text} hits={block.hits} />
      </pre>
      {block.truncated && <p className="text-xs text-muted">This message is truncated.</p>}
    </div>
  );
}

/** Search, read, and restore archived SessionWiki transcripts. */
export function WikiPanel({ workspaceId, wikiId }: WikiPanelProps) {
  const meta = useSessionStore(state => state.meta);
  const pushToast = useUiStore(state => state.pushToast);
  const navigate = useUiStore(state => state.navigate);

  const [query, setQuery] = useState('');
  const [results, setResults] = useState<WikiRow[]>([]);
  const [searchStatus, setSearchStatus] = useState<WikiIndexState | undefined>(undefined);
  const [searching, setSearching] = useState(false);
  const [searchError, setSearchError] = useState('');
  const [searchRan, setSearchRan] = useState(false);

  const [selectedId, setSelectedId] = useState(wikiId ?? '');
  const [selectedRow, setSelectedRow] = useState<WikiRow | null>(null);
  const [hitQuery, setHitQuery] = useState('');
  const [info, setInfo] = useState<WikiSessionInfo | null>(null);
  const [brief, setBrief] = useState<WikiBrief | null>(null);
  const [hits, setHits] = useState<WikiHitTranscript | null>(null);
  const [detailLoading, setDetailLoading] = useState(false);
  const [detailError, setDetailError] = useState('');
  const [briefError, setBriefError] = useState('');
  const [hitsError, setHitsError] = useState('');

  const [restoreProfile, setRestoreProfile] = useState('');
  const [restoreTarget, setRestoreTarget] = useState('');
  const [restoreDirectory, setRestoreDirectory] = useState('');
  const [restoreModel, setRestoreModel] = useState('');
  const [restoreEffort, setRestoreEffort] = useState('');
  const [restoring, setRestoring] = useState(false);
  const [restoreError, setRestoreError] = useState('');

  const [syncing, setSyncing] = useState(false);
  const [syncStatus, setSyncStatus] = useState<WikiSyncStatus | null>(null);
  const [syncError, setSyncError] = useState('');

  const searchController = useRef<AbortController | null>(null);
  const syncController = useRef<AbortController | null>(null);

  useEffect(() => {
    if (wikiId) {
      setSelectedId(wikiId);
      setSelectedRow(null);
      setHitQuery('');
    }
  }, [wikiId]);

  const runSearch = useCallback(async (text: string) => {
    searchController.current?.abort();
    const controller = new AbortController();
    searchController.current = controller;
    setSearching(true);
    setSearchError('');
    setSearchRan(true);
    try {
      const page = await searchWiki(text.trim(), undefined, controller.signal);
      if (controller.signal.aborted) return;
      setResults(page.rows);
      setSearchStatus(page.status.state);
    } catch (error) {
      if (!controller.signal.aborted) setSearchError(describe(error));
    } finally {
      if (searchController.current === controller) {
        searchController.current = null;
        setSearching(false);
      }
    }
  }, []);

  useEffect(() => {
    if (wikiId) return undefined;
    const timer = window.setTimeout(() => void runSearch(query), 250);
    return () => window.clearTimeout(timer);
  }, [query, wikiId, runSearch]);

  useEffect(() => () => {
    searchController.current?.abort();
    syncController.current?.abort();
  }, []);

  useEffect(() => {
    if (!selectedId) {
      setInfo(null);
      setBrief(null);
      setHits(null);
      return undefined;
    }
    const controller = new AbortController();
    setDetailLoading(true);
    setDetailError('');
    setBriefError('');
    setHitsError('');
    setInfo(null);
    setBrief(null);
    setHits(null);
    const requests: Promise<unknown>[] = [
      getWikiSession(selectedId, controller.signal)
        .then(setInfo)
        .catch(error => {
          if (!controller.signal.aborted) setDetailError(describe(error));
        }),
      getWikiBrief(selectedId, undefined, controller.signal)
        .then(setBrief)
        .catch(error => {
          if (!controller.signal.aborted) setBriefError(describe(error));
        }),
    ];
    if (hitQuery) {
      requests.push(
        getWikiHits(selectedId, { q: hitQuery }, controller.signal)
          .then(setHits)
          .catch(error => {
            if (!controller.signal.aborted) setHitsError(describe(error));
          }),
      );
    }
    void Promise.allSettled(requests).finally(() => {
      if (!controller.signal.aborted) setDetailLoading(false);
    });
    return () => controller.abort();
  }, [selectedId, hitQuery]);

  useEffect(() => {
    if (!info) return;
    const profiles = meta?.profiles ?? [];
    const targets = meta?.targets ?? [];
    setRestoreProfile(
      info.profile_id && profiles.some(profile => profile.id === info.profile_id)
        ? info.profile_id
        : (profiles.find(profile => profile.availability.status === 'available')?.id ??
            profiles[0]?.id ??
            ''),
    );
    setRestoreTarget(
      info.target_template_id && targets.some(target => target.id === info.target_template_id)
        ? info.target_template_id
        : (targets.find(target => target.availability.status !== 'missing')?.id ??
            targets[0]?.id ??
            ''),
    );
    setRestoreDirectory('');
    setRestoreModel('');
    setRestoreEffort('');
    setRestoreError('');
  }, [info, meta]);

  const capabilityChoices = meta?.profile_capabilities?.profiles?.[restoreProfile]?.choices;
  const models = capabilityChoices?.status === 'ready' ? capabilityChoices.value.models : [];
  const efforts = capabilityChoices?.status === 'ready' ? capabilityChoices.value.efforts : [];

  const openRow = useCallback(
    (row: WikiRow) => {
      setSelectedRow(row);
      setSelectedId(row.id);
      setHitQuery(query.trim());
    },
    [query],
  );

  const closeDetail = useCallback(() => {
    setSelectedId('');
    setSelectedRow(null);
    setHitQuery('');
  }, []);

  const runSync = useCallback(async () => {
    if (syncing) return;
    setSyncing(true);
    setSyncError('');
    setSyncStatus(null);
    const controller = new AbortController();
    syncController.current = controller;
    try {
      let status = await startWikiSync();
      if (controller.signal.aborted) return;
      setSyncStatus(status);
      while (!controller.signal.aborted && (status.running !== null || status.outcome === null)) {
        const next = await watchWikiSync(status.epoch, status.version, controller.signal);
        if (controller.signal.aborted) return;
        status = next;
        setSyncStatus(next);
      }
      if (status.outcome && status.outcome !== 'succeeded') {
        setSyncError(`Sync failed: ${status.outcome.failed}`);
        return;
      }
      pushToast('success', 'SessionWiki index synced.');
      await runSearch(query);
    } catch (error) {
      if (!controller.signal.aborted) setSyncError(describe(error));
    } finally {
      if (!controller.signal.aborted) setSyncing(false);
    }
  }, [syncing, pushToast, runSearch, query]);

  const restore = useCallback(async () => {
    if (!selectedId || !restoreProfile || !restoreTarget || restoring) return;
    setRestoring(true);
    setRestoreError('');
    const body: WikiRestoreBody = {
      workspace_id: workspaceId,
      profile_id: restoreProfile,
      target_id: restoreTarget,
    };
    if (restoreDirectory.trim()) body.project_directory = restoreDirectory.trim();
    if (restoreModel) body.model = restoreModel;
    if (restoreEffort) body.effort = restoreEffort;
    try {
      const result = await restoreWikiSession(selectedId, body);
      pushToast('success', 'Restored the archived session.');
      navigate({ name: 'conversation', sessionId: result.session_id });
    } catch (error) {
      setRestoreError(describe(error));
    } finally {
      setRestoring(false);
    }
  }, [
    selectedId,
    restoreProfile,
    restoreTarget,
    restoring,
    workspaceId,
    restoreDirectory,
    restoreModel,
    restoreEffort,
    pushToast,
    navigate,
  ]);

  const statusNote = wikiStatusNote(searchStatus);
  const title = info?.title || selectedRow?.title || selectedId;
  const project = info?.project || selectedRow?.project || '';
  const tool = info?.tool || selectedRow?.tool || '';
  const canRestore = Boolean(
    info &&
      info.nothing_to_restore !== true &&
      restoreProfile &&
      restoreTarget &&
      !restoring,
  );

  const targetOptions = useMemo(() => meta?.targets ?? [], [meta]);

  return (
    <div className="flex flex-col gap-3">
      <div className="flex flex-wrap items-center gap-2">
        <button
          type="button"
          disabled={syncing}
          onClick={() => void runSync()}
          className="inline-flex min-h-9 items-center gap-1.5 rounded-md border border-line px-3 text-sm text-fg hover:bg-raised disabled:opacity-50"
        >
          {syncing ? <LoaderCircle className="size-4 animate-spin" aria-hidden /> : <RefreshCw className="size-4" aria-hidden />}
          Sync index
        </button>
        {syncStatus && <span className="text-xs text-muted">{syncProgress(syncStatus)}</span>}
      </div>
      {syncError && (
        <p className="text-sm text-danger" role="alert">
          {syncError}
        </p>
      )}

      {!selectedId && (
        <>
          <form
            onSubmit={event => {
              event.preventDefault();
              void runSearch(query);
            }}
            className="flex items-center gap-2"
            role="search"
          >
            <label className="flex min-w-0 flex-1 flex-col gap-1 text-xs text-muted">
              <span>Search the SessionWiki</span>
              <input
                value={query}
                onChange={event => setQuery(event.target.value)}
                placeholder="Project, title, or message text"
                className="min-w-0 rounded-md border border-line bg-bg px-2.5 py-2 text-sm text-fg placeholder:text-muted/70"
              />
            </label>
            <button
              type="submit"
              className="inline-flex min-h-9 items-center gap-1.5 self-end rounded-md bg-accent px-3 text-sm font-medium text-accent-fg"
            >
              <Search className="size-4" aria-hidden />
              Search
            </button>
          </form>
          {statusNote && <p className="text-xs text-warn">{statusNote}</p>}
          {searching && (
            <p className="flex items-center gap-2 text-sm text-muted" role="status">
              <LoaderCircle className="size-4 animate-spin" aria-hidden />
              Searching the archive…
            </p>
          )}
          {searchError && (
            <p className="text-sm text-danger" role="alert">
              {searchError}
            </p>
          )}
          <ul className="flex flex-col gap-1.5" aria-label="SessionWiki results">
            {results.map(row => (
              <li key={row.id}>
                <button
                  type="button"
                  onClick={() => openRow(row)}
                  className="flex w-full flex-col rounded-lg border border-line bg-raised px-3 py-2 text-left hover:bg-surface"
                >
                  <span className="flex items-baseline justify-between gap-2">
                    <span className="truncate text-sm font-medium text-fg">{row.title || row.id}</span>
                    <span className="shrink-0 text-xs text-muted">
                      {row.tool} · {row.msgs} message{row.msgs === 1 ? '' : 's'}
                      {row.archived ? ' · archived' : ''}
                    </span>
                  </span>
                  <span className="truncate text-xs text-muted">
                    {row.project}
                    {row.started ? ` · started ${row.started}` : ''}
                  </span>
                  {(row.snippet || row.preview) && (
                    <span className="mt-0.5 line-clamp-2 text-xs text-fg/80">{row.snippet || row.preview}</span>
                  )}
                </button>
              </li>
            ))}
          </ul>
          {!searching && !searchError && searchRan && !results.length && (
            <p className="rounded-lg border border-line bg-surface p-4 text-sm text-muted">
              No archived sessions match this search.
            </p>
          )}
        </>
      )}

      {selectedId && (
        <article className="flex flex-col gap-3 rounded-xl border border-line bg-surface p-4">
          <div className="flex items-center justify-between gap-2">
            <button
              type="button"
              onClick={closeDetail}
              className="inline-flex min-h-8 items-center gap-1.5 rounded-md border border-line px-2.5 text-xs text-fg hover:bg-raised"
            >
              <ArrowLeft className="size-3.5" aria-hidden />
              Back to search
            </button>
            {info?.status && <span className="text-xs text-muted">{info.status}</span>}
          </div>
          <div>
            <h2 className="text-base font-semibold text-fg">{title || 'Archived session'}</h2>
            <p className="mt-0.5 text-xs text-muted">
              {[project, tool, info?.status].filter(Boolean).join(' · ')}
            </p>
          </div>

          {detailLoading && (
            <p className="flex items-center gap-2 text-sm text-muted" role="status">
              <LoaderCircle className="size-4 animate-spin" aria-hidden />
              Loading the archived session…
            </p>
          )}
          {detailError && (
            <p className="text-sm text-danger" role="alert">
              {detailError}
            </p>
          )}
          {info?.nothing_to_restore && (
            <p className="rounded-lg border border-warn/40 bg-warn/10 p-3 text-sm text-warn">
              This transcript has no restorable conversation content.
            </p>
          )}

          <section className="flex flex-col gap-1.5">
            <h3 className="text-sm font-semibold text-fg">Brief</h3>
            {briefError && (
              <p className="text-sm text-danger" role="alert">
                {briefError}
              </p>
            )}
            {brief ? (
              <div className="rounded-lg border border-line bg-raised p-3">
                <MarkdownContent text={brief.markdown} />
              </div>
            ) : (
              !briefError && <p className="text-xs text-muted">Loading the briefing…</p>
            )}
          </section>

          {hitQuery && (
            <section className="flex flex-col gap-1.5">
              <h3 className="text-sm font-semibold text-fg">Search hits for “{hitQuery}”</h3>
              {hitsError && (
                <p className="text-sm text-danger" role="alert">
                  {hitsError}
                </p>
              )}
              {hits?.blocks.map((block, index) => (
                <HitBlockView key={index} block={block} />
              ))}
              {(hits?.omitted_after ?? 0) > 0 && (
                <p className="text-xs text-muted">… {hits?.omitted_after} later messages omitted</p>
              )}
              {hits && !hits.blocks.length && <p className="text-sm text-muted">No matching passages.</p>}
            </section>
          )}

          <section className="flex flex-col gap-2">
            <h3 className="text-sm font-semibold text-fg">Restore</h3>
            <p className="text-xs text-muted">
              Restoring starts a new session that opens with a summary of this archived conversation.
            </p>
            <div className="grid gap-2 sm:grid-cols-2">
              <label className="flex flex-col gap-1 text-xs text-muted">
                <span>Profile</span>
                <select
                  value={restoreProfile}
                  onChange={event => {
                    setRestoreProfile(event.target.value);
                    setRestoreModel('');
                    setRestoreEffort('');
                  }}
                  className="rounded-md border border-line bg-bg px-2.5 py-2 text-sm text-fg"
                >
                  {!meta?.profiles.length && <option value="">No profiles configured</option>}
                  {meta?.profiles.map(profile => (
                    <option key={profile.id} value={profile.id}>
                      {profile.id} ({profile.harness_kind})
                    </option>
                  ))}
                </select>
              </label>
              <label className="flex flex-col gap-1 text-xs text-muted">
                <span>Target</span>
                <select
                  value={restoreTarget}
                  onChange={event => setRestoreTarget(event.target.value)}
                  className="rounded-md border border-line bg-bg px-2.5 py-2 text-sm text-fg"
                >
                  {!targetOptions.length && <option value="">No targets configured</option>}
                  {targetOptions.map(target => (
                    <option key={target.id} value={target.id} disabled={target.availability.status === 'missing'}>
                      {target.id} ({target.kind})
                    </option>
                  ))}
                </select>
              </label>
              <label className="flex flex-col gap-1 text-xs text-muted sm:col-span-2">
                <span>Project directory (optional)</span>
                <input
                  value={restoreDirectory}
                  onChange={event => setRestoreDirectory(event.target.value)}
                  placeholder="Only for targets that open an existing directory"
                  spellCheck={false}
                  className="rounded-md border border-line bg-bg px-2.5 py-2 font-mono text-sm text-fg"
                />
              </label>
              <label className="flex flex-col gap-1 text-xs text-muted">
                <span>Model</span>
                <select
                  value={restoreModel}
                  onChange={event => setRestoreModel(event.target.value)}
                  className="rounded-md border border-line bg-bg px-2.5 py-2 text-sm text-fg"
                >
                  <option value="">Profile default</option>
                  {models.map(choice => (
                    <option key={choice.value} value={choice.value}>
                      {choice.name || choice.value}
                    </option>
                  ))}
                </select>
              </label>
              <label className="flex flex-col gap-1 text-xs text-muted">
                <span>Effort</span>
                <select
                  value={restoreEffort}
                  onChange={event => setRestoreEffort(event.target.value)}
                  className="rounded-md border border-line bg-bg px-2.5 py-2 text-sm text-fg"
                >
                  <option value="">Profile default</option>
                  {efforts.map(choice => (
                    <option key={choice.value} value={choice.value}>
                      {choice.name || choice.value}
                    </option>
                  ))}
                </select>
              </label>
            </div>
            {restoreError && (
              <p className="text-sm text-danger" role="alert">
                {restoreError}
              </p>
            )}
            <button
              type="button"
              disabled={!canRestore}
              onClick={() => void restore()}
              className="inline-flex min-h-10 w-full items-center justify-center gap-2 rounded-lg bg-accent px-4 text-sm font-semibold text-accent-fg disabled:opacity-50"
            >
              {restoring && <LoaderCircle className="size-4 animate-spin" aria-hidden />}
              Restore session
            </button>
          </section>
        </article>
      )}
    </div>
  );
}
