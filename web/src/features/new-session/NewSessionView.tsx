import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { FolderSearch, LoaderCircle, RefreshCw, Trash2 } from 'lucide-react';
import {
  createBundle,
  deleteBundle,
  discoverProjects,
  getLaunchOptions,
  getProjects,
  getProfileProviders,
  selectProviderProfile,
  getSubagentOptions,
  preflightNew,
} from '../../api/client';
import {
  serializeAdditionalMount,
  type AdditionalMount,
  type CreateBundleRequest,
  type LaunchAvailability,
  type LaunchOptions,
  type LaunchProfile,
  type LaunchTarget,
  type LocalRemoteRepair,
  type NewSessionAction,
  type NewSessionPreflightRequest,
  type PreflightNew,
  type ProjectCatalogView,
  type ProjectDiscovery,
  type ProjectEntry,
  type QuotaProvider,
  type SessionReview,
  type SubagentChoice,
  type SubagentOptions,
  type SubagentPolicy,
  type ViewerProfile,
  type ViewerTarget,
} from '../../api/types';
import { useSessionStore } from '../../stores/useSessionStore';
import { useUiStore } from '../../stores/useUiStore';
import { MountsEditor } from './MountsEditor';
import { PathInput } from './PathInput';
import { GIB, formatGiB, parseGiB } from '../gib';

/** Props the shell passes. Keep this shape when replacing the placeholder. */
export type NewSessionViewProps = { workspaceId: string };

const MIN_CPUS = 2;
const MIN_MEMORY = 4 * GIB;

type DelegationMode = 'default' | 'native' | 'mjolnir' | 'none';
type ReviewMode = 'inherit' | 'push' | 'off';

interface BundleChoice {
  id: string;
  label: string;
  repositories: string[];
  saved: boolean;
  search?: string;
}

function providerLabel(provider: QuotaProvider): string {
  if (typeof provider !== 'string') return provider.Other;
  if (provider === 'XAI') return 'xAI';
  if (provider === 'Zai') return 'Z.ai';
  return provider;
}

function describe(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function targetAvailability(target: LaunchTarget | undefined): {
  status: LaunchAvailability;
  reason?: string;
} {
  if (!target) return { status: 'unknown' };
  return { status: target.availability, reason: target.unavailable_reason };
}

function profileAvailability(profile: ViewerProfile | undefined): {
  status: LaunchAvailability;
  reason?: string;
} {
  if (!profile) return { status: 'unknown' };
  switch (profile.availability.status) {
    case 'available':
      return { status: 'ready' };
    case 'missing':
      return { status: 'unavailable', reason: profile.availability.reason };
    default:
      return { status: 'unknown' };
  }
}

const BADGE_CLASS: Record<LaunchAvailability, string> = {
  ready: 'border-ok/40 bg-ok/10 text-ok',
  stale: 'border-warn/40 bg-warn/10 text-warn',
  unavailable: 'border-danger/40 bg-danger/10 text-danger',
  unknown: 'border-line bg-raised text-muted',
};

function AvailabilityBadge({
  status,
  reason,
}: {
  status: LaunchAvailability;
  reason?: string;
}) {
  return (
    <span className="flex flex-col gap-0.5">
      <span className={`inline-flex w-fit items-center rounded-full border px-2 py-0.5 text-[11px] font-medium ${BADGE_CLASS[status]}`}>
        {status}
      </span>
      {reason && <span className="text-xs text-warn">{reason}</span>}
    </span>
  );
}

function Section({
  title,
  description,
  children,
}: {
  title: string;
  description?: string;
  children: ReactNode;
}) {
  return (
    <section className="flex flex-col gap-3 rounded-xl border border-line bg-surface p-4">
      <div>
        <h2 className="text-sm font-semibold text-fg">{title}</h2>
        {description && <p className="mt-0.5 text-xs text-muted">{description}</p>}
      </div>
      {children}
    </section>
  );
}

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <label className="flex min-w-0 flex-col gap-1 text-xs text-muted">
      <span>{label}</span>
      {children}
    </label>
  );
}

export default function NewSessionView({ workspaceId }: NewSessionViewProps) {
  const meta = useSessionStore(state => state.meta);
  const sessions = useSessionStore(state => state.sessionsById);
  const dispatchAction = useSessionStore(state => state.dispatchAction);
  const pushToast = useUiStore(state => state.pushToast);
  const navigate = useUiStore(state => state.navigate);

  const [options, setOptions] = useState<LaunchOptions | null>(null);
  const [projects, setProjects] = useState<ProjectCatalogView | null>(null);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState('');

  const [profileId, setProfileId] = useState('');
  const [providers, setProviders] = useState<Record<string, QuotaProvider>>({});
  const [providerLookupError, setProviderLookupError] = useState<string | null>(null);
  const [providersLoaded, setProvidersLoaded] = useState(false);
  const [providersReloadNonce, setProvidersReloadNonce] = useState(0);
  const [providerSelectRetryNonce, setProviderSelectRetryNonce] = useState(0);
  const [autoProvider, setAutoProvider] = useState<QuotaProvider | null>(null);
  const [resolvedTarget, setResolvedTarget] = useState<string | null>(null);
  const [providerError, setProviderError] = useState('');
  const [expandedProviders, setExpandedProviders] = useState<string[]>([]);
  const [bundleSearch, setBundleSearch] = useState('');
  const [focusedBundleId, setFocusedBundleId] = useState<string | null>(null);
  const [createOpen, setCreateOpen] = useState<boolean | null>(null);
  const [targetId, setTargetId] = useState('');
  const [bundleId, setBundleId] = useState('');
  const [projectDirectory, setProjectDirectory] = useState('');

  const [cpuText, setCpuText] = useState('');
  const [memoryText, setMemoryText] = useState('');
  const [rememberSize, setRememberSize] = useState(false);
  const [mounts, setMounts] = useState<AdditionalMount[]>([]);

  const [preflight, setPreflight] = useState<PreflightNew | null>(null);
  const [preflightLoading, setPreflightLoading] = useState(false);
  const [preflightError, setPreflightError] = useState('');
  const [appliedKey, setAppliedKey] = useState('');
  const [acknowledgement, setAcknowledgement] = useState<Record<string, boolean>>({});

  const [createManagedWorktree, setCreateManagedWorktree] = useState(false);
  const [at, setAt] = useState('');
  const [branch, setBranch] = useState('');
  const [base, setBase] = useState('');
  const [noLaterHistory, setNoLaterHistory] = useState(false);
  const [title, setTitle] = useState('');

  const [delegationMode, setDelegationMode] = useState<DelegationMode>('default');
  const [straightforwardModel, setStraightforwardModel] = useState('');
  const [straightforwardEffort, setStraightforwardEffort] = useState('');
  const [subtleModel, setSubtleModel] = useState('');
  const [subtleEffort, setSubtleEffort] = useState('');
  const [orchestrators, setOrchestrators] = useState(false);
  const [subagentOptions, setSubagentOptions] = useState<SubagentOptions | null>(null);
  const [subagentOptionsLoading, setSubagentOptionsLoading] = useState(false);
  const [subagentOptionsError, setSubagentOptionsError] = useState('');

  const [oracleEnabled, setOracleEnabled] = useState(false);
  const [oracleModel, setOracleModel] = useState('');
  const [oracleEffort, setOracleEffort] = useState('');
  const [reviewMode, setReviewMode] = useState<ReviewMode>('inherit');

  const [noProjectMemory, setNoProjectMemory] = useState(false);
  const [noGithubEvents, setNoGithubEvents] = useState(false);
  const [noGithubCredentials, setNoGithubCredentials] = useState(false);
  const [notificationsIgnored, setNotificationsIgnored] = useState(false);

  const [newSource, setNewSource] = useState('');
  const [includeProject, setIncludeProject] = useState(true);
  const [creatingBundle, setCreatingBundle] = useState(false);
  const [createdBundles, setCreatedBundles] = useState<BundleChoice[]>([]);
  const [projectError, setProjectError] = useState('');

  const [discoveryOpen, setDiscoveryOpen] = useState(false);
  const [discoveryPath, setDiscoveryPath] = useState('');
  const [discovery, setDiscovery] = useState<ProjectDiscovery | null>(null);
  const [discoveryLoading, setDiscoveryLoading] = useState(false);
  const [discoveryError, setDiscoveryError] = useState('');

  const [launching, setLaunching] = useState(false);
  const [launchError, setLaunchError] = useState('');

  const initialized = useRef(false);
  const worktreeKey = useRef('');
  const sizingTarget = useRef('');
  const lastTarget = useRef('');
  const projectDirectories = useRef<Record<string, string>>({});
  const preflightController = useRef<AbortController | null>(null);
  const preflightRequestRef = useRef<NewSessionPreflightRequest | null>(null);
  const selectionRef = useRef({ targetId: '', projectDirectory: '' });
  const providerGeneration = useRef(0);

  /* ---------------------------------------------------------------------- */
  /* Lazy catalog load                                                       */
  /* ---------------------------------------------------------------------- */

  useEffect(() => {
    const controller = new AbortController();
    let alive = true;
    void (async () => {
      try {
        const [launch, catalog] = await Promise.all([
          getLaunchOptions(controller.signal),
          getProjects(controller.signal),
        ]);
        if (!alive) return;
        setOptions(launch);
        setProjects(catalog);
      } catch (error) {
        if (alive) setLoadError(describe(error));
      } finally {
        if (alive) setLoading(false);
      }
    })();
    return () => {
      alive = false;
      controller.abort();
    };
  }, []);

  useEffect(() => {
    const controller = new AbortController();
    getProfileProviders(controller.signal)
      .then(result => {
        if (controller.signal.aborted) return;
        setProviders(result.providers);
        setProviderLookupError(null);
        setProvidersLoaded(true);
      })
      .catch(error => {
        if (controller.signal.aborted) return;
        providerGeneration.current += 1;
        setAutoProvider(null);
        setProviders({});
        setProviderLookupError(describe(error));
        setProvidersLoaded(true);
      });
    return () => controller.abort();
  }, [providersReloadNonce]);

  useEffect(() => {
    if (!options || !providersLoaded || initialized.current) return;
    initialized.current = true;
    const preferredProfile = options.default?.profile_id;
    const preferredTarget = options.default?.target_id;
    setProfileId(
      preferredProfile && options.profiles.some(profile => profile.id === preferredProfile)
        ? preferredProfile
        : options.primary_configured
          ? ''
          : (options.profiles[0]?.id ?? ''),
    );
    const preferred = options.profiles.find(profile => profile.id === preferredProfile) ?? (!options.primary_configured ? options.profiles[0] : undefined);
    if (preferred && providers[preferred.id]) setExpandedProviders([JSON.stringify(providers[preferred.id])]);
    setTargetId(
      preferredTarget && options.targets.some(target => target.id === preferredTarget)
        ? preferredTarget
        : (options.targets[0]?.id ?? ''),
    );
  }, [options, providers, providersLoaded]);

  useEffect(() => {
    if (!autoProvider) return;
    const controller = new AbortController();
    const generation = ++providerGeneration.current;
    setResolvedTarget(null);
    setProviderError('');
    selectProviderProfile({ provider: autoProvider, profile_id: null, target_template_id: targetId || null }, controller.signal)
      .then(id => { if (!controller.signal.aborted && providerGeneration.current === generation) { setProfileId(id); setResolvedTarget(targetId); } })
      .catch(error => { if (!controller.signal.aborted && providerGeneration.current === generation) setProviderError(describe(error)); });
    return () => controller.abort();
  }, [autoProvider, targetId, providerSelectRetryNonce]);

  useEffect(() => {
    if (!initialized.current || bundleId) return;
    const first = options?.default?.bundle_id ?? options?.bundles[0]?.id ?? projects?.projects[0]?.bundle_id ?? '';
    if (first) setBundleId(first);
  }, [options, projects, bundleId, providersLoaded]);

  const viewerTarget = useMemo<ViewerTarget | undefined>(
    () => meta?.targets.find(target => target.id === targetId),
    [meta, targetId],
  );
  const viewerProfile = useMemo<ViewerProfile | undefined>(
    () => meta?.profiles.find(profile => profile.id === profileId),
    [meta, profileId],
  );
  const launchTarget = useMemo<LaunchTarget | undefined>(
    () => options?.targets.find(target => target.id === targetId),
    [options, targetId],
  );
  // An empty profile asks the daemon to pick among the configured primary models.
  const autoPrimary = !autoProvider && options?.primary_configured === true && profileId === '';
  const providerPending = autoProvider !== null && resolvedTarget !== targetId;
  const providerGroups = useMemo(() => {
    const groups = new Map<string, { provider: QuotaProvider; profiles: LaunchProfile[] }>();
    for (const profile of options?.profiles ?? []) {
      const provider = providers[profile.id];
      if (!provider) continue;
      const key = JSON.stringify(provider);
      const group = groups.get(key) ?? { provider, profiles: [] };
      group.profiles.push(profile);
      groups.set(key, group);
    }
    return [...groups.entries()];
  }, [options, providers]);
  const ungroupedProfiles = (options?.profiles ?? []).filter(profile => !providers[profile.id]);
  const providerNotice = providerLookupError ?? (providersLoaded && ungroupedProfiles.length
    ? `No provider returned for ${ungroupedProfiles.map(profile => profile.id).join(', ')}.`
    : null);

  useEffect(() => {
    if (!providersLoaded) return;
    if (autoProvider && !providerGroups.some(([, group]) => JSON.stringify(group.provider) === JSON.stringify(autoProvider))) {
      providerGeneration.current += 1;
      setAutoProvider(null);
    }
  }, [autoProvider, providerGroups, providersLoaded]);

  useEffect(() => {
    if (autoProvider || !providers[profileId]) return;
    const key = JSON.stringify(providers[profileId]);
    setExpandedProviders(current => current.includes(key) ? current : [...current, key]);
  }, [providers, profileId, autoProvider]);

  const launchProfile = useMemo(
    () => options?.profiles.find(profile => profile.id === profileId),
    [options, profileId],
  );

  const requiresProjectDirectory =
    launchTarget?.requires_project_directory ?? viewerTarget?.requires_project_directory ?? false;
  const resourceKind = viewerTarget?.resource_allocation_kind ?? 'fixed';

  const bundleChoices = useMemo<BundleChoice[]>(() => {
    const choices = new Map<string, BundleChoice>();
    for (const bundle of options?.bundles ?? []) {
      choices.set(bundle.id, {
        id: bundle.id,
        label: bundle.id,
        repositories: bundle.repositories.map(repository => repository.github || repository.id),
        search: [bundle.primary_repository, ...bundle.repositories.flatMap(repository => [repository.id, repository.github ?? '', repository.destination])].join(' '),
        saved: true,
      });
    }
    for (const project of projects?.projects ?? []) {
      const existing = choices.get(project.bundle_id);
      choices.set(project.bundle_id, {
        id: project.bundle_id,
        label: project.name || existing?.label || project.bundle_id,
        repositories: project.project.bundle.repositories.map(
          repository => repository.github || repository.id,
        ),
        search: [existing?.search ?? '', project.project.bundle.primary_repo, ...project.project.bundle.repositories.flatMap(repository => [repository.id, repository.github ?? '', repository.destination])].join(' '),
        saved: true,
      });
    }
    for (const bundle of createdBundles) {
      if (!choices.has(bundle.id)) choices.set(bundle.id, bundle);
    }
    const recent = new Map<string, number>();
    const sorted = Object.values(sessions).sort((a, b) => (b.last_activity_at_ms ?? (Date.parse(b.updated_at) || 0)) - (a.last_activity_at_ms ?? (Date.parse(a.updated_at) || 0)));
    for (const row of sorted) if (!recent.has(row.bundle_id)) recent.set(row.bundle_id, recent.size);
    if (options?.default?.bundle_id && !recent.has(options.default.bundle_id)) recent.set(options.default.bundle_id, recent.size);
    return [...choices.values()].sort((a, b) => (recent.get(a.id) ?? Infinity) - (recent.get(b.id) ?? Infinity) || a.label.localeCompare(b.label));
  }, [options, projects, createdBundles, sessions]);
  const recentBundleIds = useMemo(() => new Set([...Object.values(sessions).map(row => row.bundle_id), options?.default?.bundle_id].filter(Boolean)), [sessions, options]);
  const filteredBundles = bundleChoices.filter(bundle => `${bundle.id} ${bundle.label} ${bundle.repositories.join(' ')} ${bundle.search ?? ''}`.toLowerCase().includes(bundleSearch.toLowerCase()));
  const selectedBundle = bundleChoices.find(bundle => bundle.id === bundleId);
  const tabBundleId = filteredBundles.find(bundle => bundle.id === focusedBundleId)?.id ??
    filteredBundles.find(bundle => bundle.id === bundleId)?.id ?? filteredBundles[0]?.id;

  const completeMounts = useMemo(
    () => mounts.filter(mount => mount.source.trim() && mount.destination.trim()),
    [mounts],
  );

  /* ---------------------------------------------------------------------- */
  /* Sizing                                                                  */
  /* ---------------------------------------------------------------------- */

  useEffect(() => {
    if (!targetId || !meta || sizingTarget.current === targetId) return;
    sizingTarget.current = targetId;
    const target = meta.targets.find(item => item.id === targetId);
    if (target?.resource_allocation_kind !== 'container') {
      setCpuText('');
      setMemoryText('');
      setRememberSize(false);
      return;
    }
    const remembered = target.remembered_container_size;
    const fallback = target.default_resource_allocation;
    const cpus = remembered?.cpus ?? (fallback?.kind === 'container' ? fallback.cpus : MIN_CPUS);
    const memory =
      remembered?.memory_bytes ?? (fallback?.kind === 'container' ? fallback.memory_bytes : MIN_MEMORY);
    setCpuText(String(cpus));
    setMemoryText(formatGiB(memory));
    setRememberSize(false);
  }, [targetId, meta]);

  useEffect(() => {
    if (!targetId || !meta || lastTarget.current === targetId) return;
    lastTarget.current = targetId;
    const target = meta.targets.find(item => item.id === targetId);
    if (!target?.requires_project_directory) {
      setProjectDirectory('');
      return;
    }
    setProjectDirectory(
      projectDirectories.current[targetId] ?? target.recent_project_directories?.[0] ?? '',
    );
  }, [targetId, meta]);

  const setDirectory = useCallback(
    (value: string) => {
      setProjectDirectory(value);
      if (targetId) projectDirectories.current[targetId] = value;
    },
    [targetId],
  );

  const allocation = useMemo(() => {
    if (resourceKind !== 'container') return null;
    const cpus = Number(cpuText);
    const memory = parseGiB(memoryText);
    if (!Number.isInteger(cpus) || cpus < 1 || memory === null) return null;
    return { kind: 'container' as const, cpus, memory_bytes: memory };
  }, [resourceKind, cpuText, memoryText]);

  const allocationError = useMemo(() => {
    if (resourceKind !== 'container') return '';
    const cpus = Number(cpuText);
    if (!Number.isInteger(cpus) || cpus < MIN_CPUS) return `CPU must be a whole number of at least ${MIN_CPUS}.`;
    const memory = parseGiB(memoryText);
    if (memory === null) return 'Memory must be a positive number of GiB.';
    if (memory < MIN_MEMORY) return 'Memory must be at least 4 GiB.';
    const limit = viewerTarget?.container_capacity?.total ?? viewerTarget?.container_host_limits;
    if (limit && cpus > limit.cpus) return `CPU exceeds the ${limit.cpus} CPUs on this host.`;
    if (limit && memory > limit.memory_bytes) {
      return `Memory exceeds the ${formatGiB(limit.memory_bytes)} GiB on this host.`;
    }
    const remaining = viewerTarget?.container_capacity?.remaining;
    if (
      viewerTarget?.container_memory_hard_limit_reason &&
      (!viewerTarget.container_capacity?.complete ||
        (remaining && memory > remaining.memory_bytes))
    ) {
      return viewerTarget.container_capacity?.complete
        ? `This host has ${formatGiB(remaining?.memory_bytes ?? 0)} GiB of RAM left; choose less.`
        : 'Mjolnir cannot compute the RAM left on this host because a live container has no recorded size.';
    }
    return '';
  }, [resourceKind, cpuText, memoryText, viewerTarget]);

  /* ---------------------------------------------------------------------- */
  /* Preflight                                                               */
  /* ---------------------------------------------------------------------- */

  const preflightRequest = useMemo<NewSessionPreflightRequest | null>(() => {
    if (providerPending || (!profileId && !autoPrimary) || !targetId) return null;
    if (requiresProjectDirectory && !projectDirectory.trim()) return null;
    if (!requiresProjectDirectory && !bundleId) return null;
    const request: NewSessionPreflightRequest = {
      workspace_id: workspaceId,
      profile_id: profileId,
      bundle_id: bundleId,
      target_id: targetId,
    };
    if (requiresProjectDirectory) request.project_directory = projectDirectory.trim();
    else if (completeMounts.length) {
      request.additional_mounts = completeMounts.map(serializeAdditionalMount);
    }
    return request;
  }, [
    workspaceId,
    profileId,
    autoPrimary,
    providerPending,
    targetId,
    bundleId,
    projectDirectory,
    requiresProjectDirectory,
    completeMounts,
  ]);

  const requestKey = preflightRequest ? JSON.stringify(preflightRequest) : '';
  preflightRequestRef.current = preflightRequest;
  selectionRef.current = { targetId, projectDirectory };

  const runPreflight = useCallback(async (repairs: LocalRemoteRepair[] = []) => {
    const base = preflightRequestRef.current;
    if (!base) return;
    const request: NewSessionPreflightRequest = repairs.length
      ? { ...base, remote_repairs: repairs }
      : base;
    preflightController.current?.abort();
    const controller = new AbortController();
    preflightController.current = controller;
    setPreflightLoading(true);
    setPreflightError('');
    try {
      const result = await preflightNew(request, controller.signal);
      if (controller.signal.aborted) return;
      setPreflight(result);
      setAppliedKey(JSON.stringify(base));
      setAcknowledgement(previous => {
        const next: Record<string, boolean> = {};
        for (const repository of result.dirty_repositories) next[repository] = previous[repository] === true;
        return next;
      });
      const selectionKey = `${selectionRef.current.targetId}\u001f${selectionRef.current.projectDirectory}`;
      if (worktreeKey.current !== selectionKey) {
        worktreeKey.current = selectionKey;
        setCreateManagedWorktree(
          result.managed_worktree.available && result.managed_worktree.default_create,
        );
      }
    } catch (error) {
      if (controller.signal.aborted) return;
      setPreflight(null);
      setAppliedKey('');
      setPreflightError(describe(error));
    } finally {
      if (preflightController.current === controller) {
        preflightController.current = null;
        setPreflightLoading(false);
      }
    }
  }, []);

  useEffect(() => {
    if (!requestKey) {
      preflightController.current?.abort();
      setPreflight(null);
      setAppliedKey('');
      setPreflightLoading(false);
      return undefined;
    }
    const timer = window.setTimeout(() => void runPreflight([]), 250);
    return () => window.clearTimeout(timer);
  }, [requestKey, runPreflight]);

  useEffect(() => () => preflightController.current?.abort(), []);

  /* ---------------------------------------------------------------------- */
  /* Delegation options                                                      */
  /* ---------------------------------------------------------------------- */

  useEffect(() => {
    setSubagentOptions(null);
    setSubagentOptionsError('');
    if (delegationMode !== 'mjolnir' || !profileId) return undefined;
    const controller = new AbortController();
    setSubagentOptionsLoading(true);
    getSubagentOptions(profileId, {}, controller.signal)
      .then(result => {
        if (!controller.signal.aborted) setSubagentOptions(result);
      })
      .catch(error => {
        if (!controller.signal.aborted) setSubagentOptionsError(describe(error));
      })
      .finally(() => {
        if (!controller.signal.aborted) setSubagentOptionsLoading(false);
      });
    return () => controller.abort();
  }, [delegationMode, profileId]);

  const profileCapabilities = meta?.profile_capabilities?.profiles?.[profileId];
  const capabilityChoices =
    profileCapabilities?.choices.status === 'ready' ? profileCapabilities.choices.value : null;
  const oracleModels = capabilityChoices?.models ?? [];
  const oracleEfforts = capabilityChoices?.efforts ?? [];
  const oracleApplicable = Boolean(
    meta?.oracle_configured ||
      viewerTarget?.oracle_configured ||
      Object.hasOwn(meta?.last_oracle_choices ?? {}, profileId),
  );

  /* ---------------------------------------------------------------------- */
  /* Project actions                                                         */
  /* ---------------------------------------------------------------------- */

  const refreshCatalogs = useCallback(async () => {
    try {
      const [launch, catalog] = await Promise.all([getLaunchOptions(), getProjects()]);
      setOptions(launch);
      setProjects(catalog);
    } catch (error) {
      setProjectError(describe(error));
    }
  }, []);

  const handleDeleteBundle = useCallback(
    async (id: string) => {
      setProjectError('');
      try {
        await deleteBundle(id);
        setOptions(current =>
          current ? { ...current, bundles: current.bundles.filter(bundle => bundle.id !== id) } : current,
        );
        setProjects(current =>
          current
            ? { ...current, projects: current.projects.filter(project => project.bundle_id !== id) }
            : current,
        );
        setCreatedBundles(current => current.filter(bundle => bundle.id !== id));
        if (bundleId === id) setBundleId('');
      } catch (error) {
        setProjectError(describe(error));
      }
    },
    [bundleId],
  );

  const handleCreateBundle = useCallback(async () => {
    const sources = newSource
      .split('\n')
      .map(source => source.trim())
      .filter(Boolean);
    if (!sources.length) {
      setProjectError('Enter a Git URL, GitHub owner/repository, or a checkout path.');
      return;
    }
    setCreatingBundle(true);
    setProjectError('');
    try {
      const request: CreateBundleRequest =
        sources.length === 1
          ? { source: sources[0], include_project: includeProject }
          : { sources, include_project: includeProject };
      const result = await createBundle(request);
      setCreatedBundles(current => [
        ...current.filter(bundle => bundle.id !== result.bundle_id),
        {
          id: result.bundle_id,
          label: sources[0],
          repositories: sources,
          saved: includeProject,
        },
      ]);
      setBundleId(result.bundle_id);
      setNewSource('');
      if (result.project) {
        setProjects(current =>
          current
            ? {
                ...current,
                projects: [
                  result.project!,
                  ...current.projects.filter(project => project.bundle_id !== result.bundle_id),
                ],
              }
            : current,
        );
      }
    } catch (error) {
      setProjectError(describe(error));
    } finally {
      setCreatingBundle(false);
    }
  }, [newSource, includeProject]);

  const runDiscovery = useCallback(async (path: string) => {
    const directory = path.trim();
    if (!directory) {
      setDiscoveryError('Enter a folder to browse.');
      return;
    }
    setDiscoveryLoading(true);
    setDiscoveryError('');
    try {
      const result = await discoverProjects({ kind: 'directory', path: directory });
      setDiscovery(result);
      if (result.directory) setDiscoveryPath(result.directory);
    } catch (error) {
      setDiscoveryError(describe(error));
    } finally {
      setDiscoveryLoading(false);
    }
  }, []);

  const chooseDiscoveryEntry = useCallback((entry: ProjectEntry) => {
    if (entry.kind === 'directory') {
      setDiscoveryPath(entry.source);
      void runDiscovery(entry.source);
      return;
    }
    setNewSource(entry.source);
    setDiscoveryOpen(false);
  }, [runDiscovery]);

  /* ---------------------------------------------------------------------- */
  /* Submission                                                              */
  /* ---------------------------------------------------------------------- */

  const dirtyRepositories = preflight?.dirty_repositories ?? [];
  const dirtyAcknowledged = dirtyRepositories.every(repository => acknowledgement[repository] === true);
  const preflightFresh = preflight !== null && appliedKey === requestKey && !preflightLoading;
  const worktreeAvailable = requiresProjectDirectory && preflight?.managed_worktree.available === true;
  const mountsIncomplete = mounts.some(mount => !mount.source.trim() || !mount.destination.trim());

  const subagentPolicy = useMemo<SubagentPolicy | null>(() => {
    if (delegationMode === 'default') return null;
    if (delegationMode === 'native') return { mode: 'native' };
    if (delegationMode === 'none') return { mode: 'none' };
    const straightforward: SubagentChoice | undefined = straightforwardModel
      ? { model: straightforwardModel, ...(straightforwardEffort ? { effort: straightforwardEffort } : {}) }
      : undefined;
    const subtle: SubagentChoice | undefined = subtleModel
      ? { model: subtleModel, ...(subtleEffort ? { effort: subtleEffort } : {}) }
      : undefined;
    return {
      mode: 'mjolnir',
      ...(straightforward ? { straightforward } : {}),
      ...(subtle ? { subtle } : {}),
      ...(orchestrators ? { orchestrators: true } : {}),
    };
  }, [delegationMode, straightforwardModel, straightforwardEffort, subtleModel, subtleEffort, orchestrators]);

  const canLaunch =
    !loading &&
    !launching &&
    preflightFresh &&
    !preflightError &&
    dirtyAcknowledged &&
    !mountsIncomplete &&
    !allocationError &&
    (preflight?.remote_repairs?.length ?? 0) === 0 &&
    (!requiresProjectDirectory || Boolean(projectDirectory.trim()));

  const launch = useCallback(async () => {
    if (!canLaunch) return;
    setLaunching(true);
    setLaunchError('');
    const action: NewSessionAction = {
      action: 'new',
      workspace_id: workspaceId,
      profile_id: profileId,
      bundle_id: bundleId,
      target_id: targetId,
    };
    if (requiresProjectDirectory) action.project_directory = projectDirectory.trim();
    else if (completeMounts.length) {
      action.additional_mounts = completeMounts.map(serializeAdditionalMount);
    }
    if (dirtyRepositories.length) {
      action.dirty_ack = dirtyRepositories.filter(repository => acknowledgement[repository] === true);
    }
    if (resourceKind === 'container' && allocation) {
      action.resource_allocation = allocation;
      action.remember_container_size = rememberSize;
    }
    if (requiresProjectDirectory) {
      action.create_managed_worktree = worktreeAvailable ? createManagedWorktree : false;
    }
    if (at.trim()) action.at = at.trim();
    if (branch.trim()) action.branch = branch.trim();
    if (base.trim()) action.base = base.trim();
    if (noLaterHistory && at.trim()) action.no_later_history = true;
    if (subagentPolicy) action.subagents = subagentPolicy;
    if (oracleApplicable) action.oracle = oracleEnabled;
    if (oracleEnabled && oracleModel) {
      action.oracle_choice = {
        model: oracleModel,
        ...(oracleEffort ? { effort: oracleEffort } : {}),
      };
    }
    if (reviewMode !== 'inherit') {
      action.review = { mode: reviewMode } satisfies SessionReview;
    }
    if (noProjectMemory) action.no_project_memory = true;
    if (noGithubEvents) action.no_github_events = true;
    if (noGithubCredentials) action.no_github_credentials = true;
    if (notificationsIgnored) action.notifications_ignored = true;
    if (title.trim()) action.title = title.trim();
    try {
      await dispatchAction(action);
      pushToast('success', 'Session launch requested.');
      navigate({ name: 'workspace', workspaceId });
    } catch (error) {
      setLaunchError(describe(error));
    } finally {
      setLaunching(false);
    }
  }, [
    canLaunch,
    workspaceId,
    profileId,
    bundleId,
    targetId,
    requiresProjectDirectory,
    projectDirectory,
    completeMounts,
    dirtyRepositories,
    acknowledgement,
    resourceKind,
    allocation,
    rememberSize,
    worktreeAvailable,
    createManagedWorktree,
    at,
    branch,
    base,
    noLaterHistory,
    subagentPolicy,
    oracleApplicable,
    oracleEnabled,
    oracleModel,
    oracleEffort,
    reviewMode,
    noProjectMemory,
    noGithubEvents,
    noGithubCredentials,
    notificationsIgnored,
    title,
    dispatchAction,
    pushToast,
    navigate,
  ]);

  /* ---------------------------------------------------------------------- */
  /* Render                                                                  */
  /* ---------------------------------------------------------------------- */

  if (loading) {
    return (
      <div className="grid h-full place-items-center text-muted" role="status" aria-label="Loading launch options">
        <LoaderCircle className="size-6 animate-spin" aria-hidden />
      </div>
    );
  }

  if (loadError) {
    return (
      <section className="mx-auto flex max-w-2xl flex-col gap-3 p-4">
        <h1 className="text-lg font-semibold text-fg">New session</h1>
        <p className="text-sm text-danger" role="alert">
          {loadError}
        </p>
        <button
          type="button"
          onClick={() => {
            setLoading(true);
            setLoadError('');
            void (async () => {
              try {
                const [launch, catalog] = await Promise.all([getLaunchOptions(), getProjects()]);
                setOptions(launch);
                setProjects(catalog);
              } catch (error) {
                setLoadError(describe(error));
              } finally {
                setLoading(false);
              }
            })();
          }}
          className="inline-flex min-h-9 w-fit items-center gap-1.5 rounded-md border border-line px-3 text-sm text-fg hover:bg-raised"
        >
          <RefreshCw className="size-4" aria-hidden />
          Retry
        </button>
      </section>
    );
  }

  const targetBadge = targetAvailability(launchTarget);
  const profileBadge = profileAvailability(viewerProfile);
  const managedWorktree = preflight?.managed_worktree;
  const pendingRepairs = preflight?.remote_repairs ?? [];
  const capacity = viewerTarget?.container_capacity;
  const limits = capacity?.total ?? viewerTarget?.container_host_limits;
  const remaining = capacity?.remaining;
  const projectStatus = projects?.status;

  return (
    <section className="mx-auto flex w-full max-w-3xl flex-col gap-3 p-4 pb-[max(1rem,env(safe-area-inset-bottom))]">
      <div>
        <h1 className="text-lg font-semibold tracking-tight text-fg">New session</h1>
        <p className="mt-0.5 text-xs text-muted">
          Pick where the work runs and what it starts from. Nothing launches until you confirm.
        </p>
      </div>

      {projectStatus?.state === 'failed' && (
        <div className="rounded-lg border border-danger/40 bg-danger/10 p-3 text-sm text-danger" role="alert">
          <p>{projectStatus.errors.join('; ')}</p>
          <button
            type="button"
            onClick={() => void refreshCatalogs()}
            className="mt-2 inline-flex min-h-8 items-center gap-1.5 rounded-md border border-danger/40 px-2.5 text-xs"
          >
            <RefreshCw className="size-3.5" aria-hidden />
            Retry project catalog
          </button>
        </div>
      )}

      <Section title="1 · Target" description="Choose where the session runs.">
          <div className="flex flex-col gap-1">
            <Field label="Target">
              <select
                value={targetId}
                onChange={event => { providerGeneration.current += 1; setTargetId(event.target.value); }}
                className="rounded-md border border-line bg-bg px-2.5 py-2 text-sm text-fg"
              >
                {!options?.targets.length && <option value="">No targets configured</option>}
                {options?.targets.map(target => (
                  <option key={target.id} value={target.id} disabled={target.availability === 'unavailable'}>
                    {target.id} — {target.kind}
                  </option>
                ))}
              </select>
            </Field>
            <AvailabilityBadge status={targetBadge.status} reason={targetBadge.reason} />
          </div>
      </Section>

      {(resourceKind === 'container' || !requiresProjectDirectory) && (
        <Section
          title="Resources"
          description={
            resourceKind === 'container'
              ? 'Container size is committed on the host before the session starts.'
              : 'Attached host directories are available for this target.'
          }
        >
          {resourceKind === 'container' && (
            <div className="flex flex-col gap-2">
              <div className="grid gap-3 sm:grid-cols-2">
                <Field label="CPU cores">
                  <input
                    type="number"
                    min={MIN_CPUS}
                    step={1}
                    inputMode="numeric"
                    value={cpuText}
                    onChange={event => setCpuText(event.target.value)}
                    className="rounded-md border border-line bg-bg px-2.5 py-2 text-sm text-fg"
                  />
                </Field>
                <Field label="Memory (GiB)">
                  <input
                    type="number"
                    min={4}
                    step="any"
                    inputMode="decimal"
                    value={memoryText}
                    onChange={event => setMemoryText(event.target.value)}
                    className="rounded-md border border-line bg-bg px-2.5 py-2 text-sm text-fg"
                  />
                </Field>
              </div>
              <p className="text-xs text-muted">
                {capacity?.complete
                  ? `Host: ${capacity.total.cpus} CPUs and ${formatGiB(capacity.total.memory_bytes)} GiB RAM · left: ${capacity.remaining.cpus} CPUs and ${formatGiB(capacity.remaining.memory_bytes)} GiB RAM.`
                  : limits
                    ? `Host: ${limits.cpus} CPUs and ${formatGiB(limits.memory_bytes)} GiB RAM. RAM left is unavailable until the capacity probe answers.`
                    : 'Host capacity is not available; the daemon still enforces its provisioning limits.'}
              </p>
              {remaining && parseGiB(memoryText) !== null && (parseGiB(memoryText) ?? 0) > remaining.memory_bytes && !allocationError && (
                <p className="text-xs text-warn">
                  This asks for more RAM than the {formatGiB(remaining.memory_bytes)} GiB left on the host; it can still start.
                </p>
              )}
              {allocationError && (
                <p className="text-sm text-danger" role="alert">
                  {allocationError}
                </p>
              )}
              <label className="flex items-center gap-2 text-sm text-fg">
                <input
                  type="checkbox"
                  checked={rememberSize}
                  onChange={event => setRememberSize(event.target.checked)}
                />
                Remember this size for this host
              </label>
            </div>
          )}
          {!requiresProjectDirectory && (
            <div className="flex flex-col gap-2 border-t border-line pt-3">
              <h3 className="text-xs font-semibold uppercase tracking-wide text-muted">Additional mounts</h3>
              <MountsEditor mounts={mounts} onChange={setMounts} />
            </div>
          )}
        </Section>
      )}

      <Section
        title="2 · Project"
        description={
          requiresProjectDirectory
            ? 'This target runs directly in a directory on its host.'
            : 'Choose a saved project or create a bundle from a repository.'
        }
      >
        {requiresProjectDirectory ? (
          <>
            {(viewerTarget?.recent_project_directories?.length ?? 0) > 0 && (
              <Field label="Recent directories">
                <select
                  value=""
                  onChange={event => {
                    if (event.target.value) setDirectory(event.target.value);
                  }}
                  className="rounded-md border border-line bg-bg px-2.5 py-2 text-sm text-fg"
                >
                  <option value="">Choose a recent project…</option>
                  {viewerTarget?.recent_project_directories?.map(directory => (
                    <option key={directory} value={directory}>
                      {directory}
                    </option>
                  ))}
                </select>
              </Field>
            )}
            <PathInput
              id="new-project-directory"
              label="Project directory"
              value={projectDirectory}
              onChange={setDirectory}
              targetId={targetId}
              kind="directories"
              placeholder="~/projects/app"
              hint="The directory must exist on the selected target's host."
            />
          </>
        ) : (
          <>
            {selectedBundle && <div className="flex items-center gap-2 rounded-lg border border-line bg-raised p-3">
              <div className="min-w-0 flex-1"><p className="text-xs text-muted">Selected bundle</p><p className="truncate text-sm text-fg">{selectedBundle.id} · {selectedBundle.repositories.join(' · ')}</p></div>
              <button type="button" aria-label={`Delete bundle ${selectedBundle.id}`} onClick={() => void handleDeleteBundle(selectedBundle.id)} className="text-muted hover:text-danger"><Trash2 className="size-4" aria-hidden /></button>
            </div>}
            <input aria-label="Search bundles" placeholder="Filter bundles or repositories…" value={bundleSearch} onChange={event => setBundleSearch(event.target.value)} className="rounded-lg border border-line bg-bg px-3 py-2 text-sm text-fg" />
            <div role="listbox" aria-label="Project bundles" className="max-h-60 overflow-y-auto overscroll-contain rounded-lg border border-line"
              onBlur={event => { if (!event.currentTarget.contains(event.relatedTarget)) setFocusedBundleId(null); }}>
              {filteredBundles.map((bundle, index) => <button key={bundle.id} type="button" role="option" aria-selected={bundle.id === bundleId}
                tabIndex={bundle.id === tabBundleId ? 0 : -1}
                onFocus={() => setFocusedBundleId(bundle.id)}
                onKeyDown={event => {
                  let next = index;
                  if (event.key === 'ArrowDown') next = Math.min(index + 1, filteredBundles.length - 1);
                  else if (event.key === 'ArrowUp') next = Math.max(index - 1, 0);
                  else if (event.key === 'Home') next = 0;
                  else if (event.key === 'End') next = filteredBundles.length - 1;
                  else return;
                  event.preventDefault();
                  event.currentTarget.parentElement?.querySelectorAll<HTMLButtonElement>('[role="option"]')[next]?.focus();
                }}
                onClick={() => setBundleId(bundle.id)} className="flex w-full items-center gap-2 border-b border-line px-3 py-2 text-left last:border-b-0 hover:bg-raised">
                <span className="min-w-0 flex-1"><span className="block truncate text-sm text-fg">{bundle.label} {recentBundleIds.has(bundle.id) && <span className="text-xs text-muted">Recent</span>}</span><span className="block truncate text-xs text-muted">{bundle.id} · {bundle.repositories.join(' · ')}</span></span>
                {bundle.id === bundleId && <span aria-hidden>✓</span>}
              </button>)}
              {!filteredBundles.length && <p className="p-3 text-sm text-muted">No bundles found.</p>}
            </div>
            {(bundleSearch || bundleChoices.length > 5) && <p className="text-xs text-muted">{filteredBundles.length} of {bundleChoices.length} bundles</p>}
            <button type="button" aria-expanded={createOpen ?? bundleChoices.length === 0} onClick={() => setCreateOpen(!(createOpen ?? bundleChoices.length === 0))} className="text-left text-sm text-accent">+ Create bundle from repository</button>
            {projectError && <p className="text-sm text-danger" role="alert">{projectError}</p>}
            {(createOpen ?? bundleChoices.length === 0) && (
            <div className="flex flex-col gap-2 rounded-lg border border-line bg-raised p-3">
              <Field label="Repository source">
                <textarea
                  value={newSource}
                  onChange={event => setNewSource(event.target.value)}
                  rows={2}
                  placeholder={'One per line: owner/repo, https://…, or /path/to/checkout'}
                  spellCheck={false}
                  className="min-w-0 rounded-md border border-line bg-bg px-2.5 py-2 font-mono text-sm text-fg placeholder:text-muted/70"
                />
              </Field>
              <label className="flex items-center gap-2 text-sm text-fg">
                <input
                  type="checkbox"
                  checked={includeProject}
                  onChange={event => setIncludeProject(event.target.checked)}
                />
                Save as a project in the catalog
              </label>
              <div className="flex flex-wrap gap-2">
                <button
                  type="button"
                  disabled={creatingBundle || !newSource.trim()}
                  onClick={() => void handleCreateBundle()}
                  className="inline-flex min-h-9 items-center gap-1.5 rounded-md bg-accent px-3 text-sm font-medium text-accent-fg disabled:opacity-50"
                >
                  {creatingBundle && <LoaderCircle className="size-4 animate-spin" aria-hidden />}
                  Create bundle
                </button>
                <button
                  type="button"
                  aria-expanded={discoveryOpen}
                  onClick={() => setDiscoveryOpen(open => !open)}
                  className="inline-flex min-h-9 items-center gap-1.5 rounded-md border border-line px-3 text-sm text-fg hover:bg-surface"
                >
                  <FolderSearch className="size-4" aria-hidden />
                  Discover projects
                </button>
              </div>
              {discoveryOpen && (
                <div className="flex flex-col gap-2 border-t border-line pt-2">
                  <Field label="Folder path">
                    <input
                      value={discoveryPath}
                      onChange={event => setDiscoveryPath(event.target.value)}
                      placeholder="~/projects"
                      spellCheck={false}
                      className="rounded-md border border-line bg-bg px-2.5 py-2 font-mono text-sm text-fg"
                    />
                  </Field>
                  <div className="flex flex-wrap gap-2">
                    <button
                      type="button"
                      disabled={discoveryLoading}
                      onClick={() => void runDiscovery(discoveryPath)}
                      className="inline-flex min-h-9 items-center gap-1.5 rounded-md border border-line px-3 text-sm text-fg hover:bg-surface disabled:opacity-50"
                    >
                      {discoveryLoading && <LoaderCircle className="size-4 animate-spin" aria-hidden />}
                      Browse folder
                    </button>
                    {discovery?.parent && (
                      <button
                        type="button"
                        onClick={() => {
                          setDiscoveryPath(discovery.parent!);
                          void runDiscovery(discovery.parent!);
                        }}
                        className="inline-flex min-h-9 items-center rounded-md border border-line px-3 text-sm text-fg hover:bg-surface"
                      >
                        Up
                      </button>
                    )}
                  </div>
                  {discoveryError && (
                    <p className="text-sm text-danger" role="alert">
                      {discoveryError}
                    </p>
                  )}
                  {discovery?.entries.map(entry => (
                    <button
                      key={`${entry.kind}:${entry.source}`}
                      type="button"
                      onClick={() => chooseDiscoveryEntry(entry)}
                      className="flex min-w-0 flex-col rounded-md border border-line bg-bg px-2.5 py-1.5 text-left hover:bg-surface"
                    >
                      <span className="truncate text-sm text-fg">{entry.name}</span>
                      <span className="truncate text-xs text-muted">
                        {entry.description || entry.source}
                        {entry.kind === 'directory' ? ' · folder' : ' · repository'}
                      </span>
                    </button>
                  ))}
                  {discovery?.truncated && (
                    <p className="text-xs text-muted">More entries exist; narrow the folder path.</p>
                  )}
                </div>
              )}
            </div>
            )}
          </>
        )}
      </Section>

      <Section title="3 · Account" description="Availability is the daemon's last check.">
        <div role="group" aria-label="Profile" className="flex flex-col gap-2">
          {options?.primary_configured && <button type="button" aria-pressed={autoPrimary} onClick={() => { providerGeneration.current += 1; setAutoProvider(null); setProfileId(''); }} className="rounded-lg border border-line p-2 text-left text-sm">Auto (Primary)</button>}
          {providerGroups.map(([key, group]) => {
            const label = providerLabel(group.provider);
            const selected = autoProvider !== null && JSON.stringify(autoProvider) === key;
            const expanded = expandedProviders.includes(key);
            return <div key={key} className="rounded-lg border border-line">
              <div className="flex items-center"><button type="button" aria-pressed={selected} onClick={() => { if (!selected) { providerGeneration.current += 1; setResolvedTarget(null); setAutoProvider(group.provider); } else if (providerError || providerPending) { providerGeneration.current += 1; setProviderSelectRetryNonce(value => value + 1); } }} className="min-w-0 flex-1 p-2 text-left text-sm">Auto ({label}) {selected && (providerError ? '' : providerPending ? '· Resolving…' : `→ ${profileId}`)}</button>
                <button type="button" aria-label={`${expanded ? 'Collapse' : 'Expand'} ${label} accounts`} aria-expanded={expanded} onClick={() => setExpandedProviders(current => expanded ? current.filter(value => value !== key) : [...current, key])} className="p-2 text-xs text-muted">{group.profiles.length} {group.profiles.length === 1 ? 'account' : 'accounts'} {expanded ? '▴' : '▾'}</button></div>
              {selected && providerError && <div role="alert" className="flex items-center gap-2 px-2 pb-2 text-xs text-danger">Could not select an account: {providerError} <button type="button" onClick={() => { providerGeneration.current += 1; setProviderSelectRetryNonce(value => value + 1); }} className="underline">Retry</button></div>}
              {expanded && group.profiles.map(profile => <button key={profile.id} type="button" aria-pressed={!autoProvider && profileId === profile.id} onClick={() => { providerGeneration.current += 1; setAutoProvider(null); setProfileId(profile.id); }} className="block w-full border-t border-line px-4 py-2 text-left text-sm">{profile.id} ({profile.harness})</button>)}
            </div>;
          })}
          {providerNotice && <div role="alert" className="text-xs text-danger">Could not load provider groups: {providerNotice} <button type="button" onClick={() => setProvidersReloadNonce(value => value + 1)} className="underline">Retry</button></div>}
          {providersLoaded && ungroupedProfiles.map(profile => <button key={profile.id} type="button" aria-pressed={!autoProvider && profileId === profile.id} onClick={() => { providerGeneration.current += 1; setAutoProvider(null); setProfileId(profile.id); }} className="rounded-lg border border-line p-2 text-left text-sm">{profile.id} ({profile.harness})</button>)}
        </div>
        {autoPrimary ? <p className="text-xs text-muted">Load-balanced across configured primary models.</p> : !autoProvider && <AvailabilityBadge status={profileBadge.status} reason={profileBadge.reason} />}
        {launchProfile && <p className="text-xs text-muted">{launchProfile.harness} via {launchTarget?.host || launchTarget?.kind || targetId}</p>}
      </Section>

      <details className="rounded-xl border border-line bg-surface p-4">
        <summary className="cursor-pointer text-sm font-semibold text-fg">Advanced options</summary>
        <div className="mt-3 flex flex-col gap-4">
          <Field label="Session title">
            <input
              value={title}
              onChange={event => setTitle(event.target.value)}
              placeholder="Derived from the project"
              className="rounded-md border border-line bg-bg px-2.5 py-2 text-sm text-fg"
            />
          </Field>

          <div className="flex flex-col gap-2">
            <h3 className="text-xs font-semibold uppercase tracking-wide text-muted">Git start point</h3>
            <div className="grid gap-2 sm:grid-cols-3">
              <Field label="At (commit)">
                <input
                  value={at}
                  onChange={event => setAt(event.target.value)}
                  spellCheck={false}
                  className="rounded-md border border-line bg-bg px-2.5 py-2 font-mono text-sm text-fg"
                />
              </Field>
              <Field label="Branch">
                <input
                  value={branch}
                  onChange={event => setBranch(event.target.value)}
                  spellCheck={false}
                  className="rounded-md border border-line bg-bg px-2.5 py-2 font-mono text-sm text-fg"
                />
              </Field>
              <Field label="Diff base">
                <input
                  value={base}
                  onChange={event => setBase(event.target.value)}
                  spellCheck={false}
                  className="rounded-md border border-line bg-bg px-2.5 py-2 font-mono text-sm text-fg"
                />
              </Field>
            </div>
            <label className="flex items-center gap-2 text-sm text-fg">
              <input
                type="checkbox"
                checked={noLaterHistory}
                onChange={event => setNoLaterHistory(event.target.checked)}
              />
              Keep only commits reachable from At and Diff base
            </label>
            <label className="flex items-center gap-2 text-sm text-fg">
              <input
                type="checkbox"
                disabled={!worktreeAvailable}
                checked={worktreeAvailable && createManagedWorktree}
                onChange={event => setCreateManagedWorktree(event.target.checked)}
              />
              Create an isolated managed checkout
            </label>
            <p className="text-xs text-muted">
              {managedWorktree?.available
                ? 'The daemon can isolate this directory into a session-owned checkout.'
                : 'A managed checkout is only offered for a directory target where the daemon supports it.'}
            </p>
          </div>

          <div className="flex flex-col gap-2">
            <h3 className="text-xs font-semibold uppercase tracking-wide text-muted">Delegation</h3>
            <Field label="Sub-agent policy">
              <select
                value={delegationMode}
                onChange={event => setDelegationMode(event.target.value as DelegationMode)}
                className="rounded-md border border-line bg-bg px-2.5 py-2 text-sm text-fg"
              >
                <option value="default">Use the profile default</option>
                <option value="native">Native</option>
                <option value="mjolnir">Mjolnir</option>
                <option value="none">None</option>
              </select>
            </Field>
            {delegationMode === 'mjolnir' && (
              <div className="flex flex-col gap-2 rounded-lg border border-line bg-raised p-3">
                {subagentOptionsLoading && (
                  <p className="text-xs text-muted" role="status">
                    Loading delegation choices…
                  </p>
                )}
                {subagentOptionsError && (
                  <p className="text-xs text-danger" role="alert">
                    {subagentOptionsError}
                  </p>
                )}
                <div className="grid gap-2 sm:grid-cols-2">
                  <ChoiceSelect
                    label="Straightforward model"
                    value={straightforwardModel}
                    choices={subagentOptions?.models ?? []}
                    onChange={setStraightforwardModel}
                  />
                  <ChoiceSelect
                    label="Straightforward effort"
                    value={straightforwardEffort}
                    choices={subagentOptions?.efforts ?? []}
                    onChange={setStraightforwardEffort}
                  />
                  <ChoiceSelect
                    label="Subtle model"
                    value={subtleModel}
                    choices={subagentOptions?.models ?? []}
                    onChange={setSubtleModel}
                  />
                  <ChoiceSelect
                    label="Subtle effort"
                    value={subtleEffort}
                    choices={subagentOptions?.efforts ?? []}
                    onChange={setSubtleEffort}
                  />
                </div>
                <label className="flex items-center gap-2 text-sm text-fg">
                  <input
                    type="checkbox"
                    checked={orchestrators}
                    onChange={event => setOrchestrators(event.target.checked)}
                  />
                  Orchestrators
                </label>
                {!meta?.orchestrators_configured && (
                  <p className="text-xs text-muted">
                    Configure orchestrator models in mj&apos;s TUI Settings → Sub-agents.
                  </p>
                )}
              </div>
            )}
          </div>

          <div className="flex flex-col gap-2">
            <h3 className="text-xs font-semibold uppercase tracking-wide text-muted">Oracle</h3>
            <label className="flex items-center gap-2 text-sm text-fg">
              <input
                type="checkbox"
                checked={oracleEnabled}
                onChange={event => setOracleEnabled(event.target.checked)}
              />
              Enable the oracle tool
            </label>
            {oracleEnabled && (
              <div className="grid gap-2 sm:grid-cols-2">
                <label className="flex flex-col gap-1 text-xs text-muted">
                  <span>Oracle model</span>
                  <select
                    value={oracleModel}
                    onChange={event => setOracleModel(event.target.value)}
                    className="rounded-md border border-line bg-bg px-2.5 py-2 text-sm text-fg"
                  >
                    <option value="">Profile default</option>
                    {oracleModels.map(choice => (
                      <option key={choice.value} value={choice.value}>
                        {choice.name || choice.value}
                      </option>
                    ))}
                  </select>
                </label>
                <label className="flex flex-col gap-1 text-xs text-muted">
                  <span>Oracle effort</span>
                  <select
                    value={oracleEffort}
                    onChange={event => setOracleEffort(event.target.value)}
                    className="rounded-md border border-line bg-bg px-2.5 py-2 text-sm text-fg"
                  >
                    <option value="">Profile default</option>
                    {oracleEfforts.map(choice => (
                      <option key={choice.value} value={choice.value}>
                        {choice.name || choice.value}
                      </option>
                    ))}
                  </select>
                </label>
              </div>
            )}
            {!oracleApplicable && (
              <p className="text-xs text-muted">The target does not advertise an oracle; the choice is left to the profile.</p>
            )}
          </div>

          <div className="flex flex-col gap-2">
            <h3 className="text-xs font-semibold uppercase tracking-wide text-muted">Review and opt-outs</h3>
            <Field label="Review mode">
              <select
                value={reviewMode}
                onChange={event => setReviewMode(event.target.value as ReviewMode)}
                className="rounded-md border border-line bg-bg px-2.5 py-2 text-sm text-fg"
              >
                <option value="inherit">Inherit configuration</option>
                <option value="push">Review on push</option>
                <option value="off">Off</option>
              </select>
            </Field>
            <div className="grid gap-1.5 sm:grid-cols-2">
              <Check checked={noProjectMemory} onChange={setNoProjectMemory} label="No project memory" />
              <Check checked={noGithubEvents} onChange={setNoGithubEvents} label="No GitHub events" />
              <Check
                checked={noGithubCredentials}
                onChange={setNoGithubCredentials}
                label="Withhold GitHub credentials"
              />
              <Check
                checked={notificationsIgnored}
                onChange={setNotificationsIgnored}
                label="Start with notifications ignored"
              />
            </div>
          </div>
        </div>
      </details>

      <Section title="4 · Preflight and launch">
        {preflightLoading && (
          <p className="flex items-center gap-2 text-sm text-muted" role="status">
            <LoaderCircle className="size-4 animate-spin" aria-hidden />
            Checking this project…
          </p>
        )}
        {preflightError && (
          <div className="rounded-lg border border-danger/40 bg-danger/10 p-3 text-sm text-danger" role="alert">
            <p>{preflightError}</p>
            <button
              type="button"
              onClick={() => void runPreflight([])}
              className="mt-2 inline-flex min-h-8 items-center gap-1.5 rounded-md border border-danger/40 px-2.5 text-xs"
            >
              <RefreshCw className="size-3.5" aria-hidden />
              Retry check
            </button>
          </div>
        )}
        {!preflightLoading && !preflightError && !preflightRequest && (
          <p className="text-sm text-muted">
            {requiresProjectDirectory
              ? 'Choose a project directory to run the preflight.'
              : 'Choose a project to run the preflight.'}
          </p>
        )}
        {pendingRepairs.length > 0 && (
          <div className="rounded-lg border border-warn/40 bg-warn/10 p-3 text-sm text-warn">
            <p className="font-medium">Git tracking needs repair before this project can launch.</p>
            <ul className="mt-1 list-disc space-y-1 pl-5 text-xs">
              {pendingRepairs.map(repair => (
                <li key={`${repair.path}:${repair.missing_remote}`}>
                  {repair.path}: {repair.branch} tracks missing remote {repair.missing_remote}. Set its
                  tracking remote to {repair.replacement_remote}.
                </li>
              ))}
            </ul>
            <button
              type="button"
              onClick={() => void runPreflight(pendingRepairs)}
              className="mt-2 inline-flex min-h-8 items-center rounded-md border border-warn/50 px-2.5 text-xs"
            >
              Repair tracking and recheck
            </button>
          </div>
        )}
        {dirtyRepositories.length > 0 && (
          <fieldset className="rounded-lg border border-warn/40 bg-warn/10 p-3">
            <legend className="px-1 text-sm font-medium text-warn">Uncommitted changes found</legend>
            <p className="text-xs text-muted">
              These repositories have local changes. Acknowledge each to launch anyway; uncommitted work
              stays in the existing checkout.
            </p>
            <div className="mt-2 flex flex-col gap-1.5">
              {dirtyRepositories.map(repository => (
                <label key={repository} className="flex items-center gap-2 text-sm text-fg">
                  <input
                    type="checkbox"
                    checked={acknowledgement[repository] === true}
                    onChange={event =>
                      setAcknowledgement(current => ({ ...current, [repository]: event.target.checked }))
                    }
                  />
                  I understand {repository} has uncommitted changes
                </label>
              ))}
            </div>
          </fieldset>
        )}
        {preflight?.review_notice && (
          <p className="rounded-lg border border-warn/40 bg-warn/10 p-3 text-sm text-warn">
            {preflight.review_notice}
          </p>
        )}
        {preflight?.local_changes_excluded && (
          <p className="rounded-lg border border-warn/40 bg-warn/10 p-3 text-sm text-warn">
            Unpublished commits and uncommitted local changes are excluded from the isolated workspace;
            only commits reachable from the published remote are used.
          </p>
        )}
        {(preflight?.remote_repositories?.length ?? 0) > 0 && (
          <div className="flex flex-col gap-1 rounded-lg border border-line bg-raised p-3 text-xs text-muted">
            <p className="font-medium text-fg">Network sources</p>
            {preflight?.remote_repositories?.map(repository => (
              <p key={repository.id}>
                <span className="text-fg">{repository.id}</span>: fetch {repository.fetch_url} ·{' '}
                {repository.default_branch} · push{' '}
                {repository.push_urls.length ? repository.push_urls.join(', ') : 'none'}
              </p>
            ))}
          </div>
        )}
        {preflightFresh && !dirtyRepositories.length && pendingRepairs.length === 0 && (
          <p className="text-sm text-ok">Preflight passed. This project is ready to launch.</p>
        )}
        {launchError && (
          <p className="text-sm text-danger" role="alert">
            {launchError}
          </p>
        )}
        <button
          type="button"
          disabled={!canLaunch}
          onClick={() => void launch()}
          className="inline-flex min-h-10 w-full items-center justify-center gap-2 rounded-lg bg-accent px-4 text-sm font-semibold text-accent-fg disabled:opacity-50"
        >
          {launching && <LoaderCircle className="size-4 animate-spin" aria-hidden />}
          Launch
        </button>
        {!canLaunch && !launching && (
          <p className="text-center text-xs text-muted">
            {preflight?.remote_repairs?.length
              ? 'Repair Git tracking before launching.'
              : dirtyRepositories.length && !dirtyAcknowledged
                ? 'Acknowledge every repository with uncommitted changes.'
                : allocationError || mountsIncomplete || !preflightFresh
                  ? 'Finish the project and resource choices above.'
                  : ''}
          </p>
        )}
      </Section>
    </section>
  );
}

function Check({
  checked,
  onChange,
  label,
}: {
  checked: boolean;
  onChange: (checked: boolean) => void;
  label: string;
}) {
  return (
    <label className="flex items-center gap-2 text-sm text-fg">
      <input type="checkbox" checked={checked} onChange={event => onChange(event.target.checked)} />
      {label}
    </label>
  );
}

function ChoiceSelect({
  label,
  value,
  choices,
  onChange,
}: {
  label: string;
  value: string;
  choices: { value: string; name: string }[];
  onChange: (value: string) => void;
}) {
  return (
    <label className="flex min-w-0 flex-col gap-1 text-xs text-muted">
      <span>{label}</span>
      <select
        value={value}
        onChange={event => onChange(event.target.value)}
        className="min-w-0 rounded-md border border-line bg-bg px-2.5 py-2 text-sm text-fg"
      >
        <option value="">Profile default</option>
        {choices.map(choice => (
          <option key={choice.value} value={choice.value}>
            {choice.name || choice.value}
          </option>
        ))}
      </select>
    </label>
  );
}

