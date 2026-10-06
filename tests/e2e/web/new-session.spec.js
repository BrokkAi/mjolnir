const { test, expect } = require('@playwright/test');
const path = require('node:path');

test.use({ viewport: { width: 390, height: 844 }, hasTouch: true, serviceWorkers: 'block' });

const defaultContainerTarget = {
  id: 'container', kind: 'podman', requires_project_directory: false, recent_project_directories: [],
  resource_allocation_kind: 'container', remembered_container_size: null,
  container_host_limits: { cpus: 24, memory_bytes: 68719476736 },
  default_resource_allocation: { kind: 'container', cpus: 8, memory_bytes: 34359738368 },
};

async function mount(page, {
  bundles = [{ id: 'existing', repositories: [] }],
  profiles = [{ id: 'alpha', harness_kind: 'codex' }, { id: 'beta', harness_kind: 'claude' }, { id: 'gamma', harness_kind: 'grok' }],
  targets = null,
  containerTarget = defaultContainerTarget,
  extraTargets = [],
  resourceOptions = {},
} = {}) {
  const state = {
    snapshot: {
      revision: 1, workspaces: [{ id: 'test', name: 'Test' }, { id: 'other', name: 'Other' }], sessions: [],
      profiles,
      targets: targets || [
        containerTarget,
        { id: 'local', kind: 'local', requires_project_directory: true, recent_project_directories: ['/work/recent', '/work/older'] },
        { id: 'remote', kind: 'ssh', requires_project_directory: true, recent_project_directories: ['/remote/recent'] },
        ...extraTargets,
      ], bundles, capacity: [], launch_failures: [],
    },
    snapshots: 0, preflights: [], preflightFailures: 0, actions: [], creates: [], rejectCreate: false,
    resourceOptionRequests: [], resourceOptions,
    completions: [], discoveries: [], discoveryFailures: 0,
    discover: body => body.kind === 'github' ? {
      entries: [{ name: 'example/app', source: 'https://github.com/example/app', description: 'A useful project', kind: 'repository' }],
      directory: null, parent: null, truncated: false,
    } : {
      entries: body.path === '/home/controller/code' ? [
        { name: 'Use code', source: '/home/controller/code', description: 'Repository in this folder', kind: 'repository' },
      ] : [
        { name: 'code', source: '/home/controller/code', description: 'Folder', kind: 'directory' },
      ], directory: body.path || '/home/controller', parent: body.path ? '/home/controller' : '/', truncated: false,
    },
    complete: () => ({ candidates: ['/work/recent/', '/work/repos/'], insert: '/work/re', truncated: false }),
    holdCreate: null, holdLaunch: null, launchResponses: 0,
    holdPreflight: null, preflightError: null, remoteRepairs: [], resolvedDirectory: null,
    worktreeOptions: { available: true, default_create: true },
  };
  const webRoot = path.resolve(__dirname, '../../../mj-controller/src/web');
  await page.addInitScript(() => {
    window.EventSource = class extends EventTarget {
      constructor() {
        super();
        window.fixtureEvents = this;
        queueMicrotask(() => this.dispatchEvent(new Event('open')));
      }
      close() {}
    };
  });
  page.on('requestfailed', request => {
    if (request.url().endsWith('/api/preflight/new')) state.preflightFailures++;
    if (request.url().endsWith('/api/projects/discover')) state.discoveryFailures++;
  });
  await page.route('**/*', async route => {
    const pathname = new URL(route.request().url()).pathname;
    const json = value => route.fulfill({ contentType: 'application/json', body: JSON.stringify(value) });
    if (pathname === '/api/snapshot') { state.snapshots++; return json(state.snapshot); }
    if (pathname === '/api/events') return route.fulfill({ contentType: 'text/event-stream', body: ': fixture\n\n' });
    const resourceOptionsMatch = /^\/api\/targets\/([^/]+)\/resource-options$/.exec(pathname);
    if (resourceOptionsMatch) {
      const targetId = decodeURIComponent(resourceOptionsMatch[1]);
      state.resourceOptionRequests.push(targetId);
      return json(state.resourceOptions[targetId]);
    }
    if (pathname === '/api/preflight/new') {
      const request = route.request().postDataJSON();
      state.preflights.push(request);
      const bare = state.snapshot.targets.find(target => target.id === request.target_id)?.requires_project_directory === true;
      if (state.holdPreflight) await state.holdPreflight;
      if (state.preflightError) return route.fulfill({ status: 400, contentType: 'application/json', body: JSON.stringify({ error: state.preflightError }) });
      return json({
        remote_repairs: state.remoteRepairs,
        project_directory: bare ? state.resolvedDirectory : null,
        remote_repositories: bare ? [] : [{ id: request.bundle_id, fetch_url: 'https://github.com/example/repo.git', default_branch: 'main', push_urls: ['https://github.com/example/repo.git'] }],
        local_changes_excluded: !bare,
        managed_worktree: bare ? state.worktreeOptions : { available: false, default_create: false },
      });
    }
    if (pathname === '/api/paths/complete') {
      const body = route.request().postDataJSON();
      state.completions.push(body);
      return json(await state.complete(body));
    }
    if (pathname === '/api/projects') {
      if (state.holdCatalog) await state.holdCatalog;
      return json({ projects: [], locations: [], status: state.catalogStatus || { state: 'ready' } });
    }
    if (pathname === '/api/projects/discover') {
      const body = route.request().postDataJSON();
      state.discoveries.push(body);
      const result = await state.discover(body);
      if (result.error) return route.fulfill({ status: 400, contentType: 'application/json', body: JSON.stringify(result) });
      return json(result);
    }
    if (pathname.endsWith('/subagent-options')) {
      const model = new URL(route.request().url()).searchParams.get('model');
      state.subagentRequests ||= [];
      state.subagentRequests.push(model);
      if (state.subagentOptions) return json(await state.subagentOptions(model));
      return json({ models: [{ value: 'model-a', name: 'Model A' }, { value: 'model-b', name: 'Model B' }], efforts: model === 'model-a' ? [{ value: 'high', name: 'High' }] : [], unavailable: [] });
    }
    if (pathname === '/api/bundles') {
      state.creates.push(route.request().postDataJSON());
      if (state.holdCreate) await state.holdCreate;
      if (state.rejectCreate) return route.fulfill({ status: 400, contentType: 'application/json', body: JSON.stringify({ error: 'Repository source is invalid' }) });
      state.snapshot.bundles.push({ id: 'created', repositories: [] });
      return json({ bundle_id: 'created' });
    }
    if (pathname === '/api/actions') {
      state.actions.push(route.request().postDataJSON());
      if (state.holdLaunch) await state.holdLaunch;
      if (state.actionError) return route.fulfill({ status: 400, contentType: 'application/json', body: JSON.stringify(state.actionError) });
      await route.fulfill({ status: 202, body: '' });
      if (state.holdLaunch) state.launchResponses += 1;
      return;
    }
    const file = pathname === '/' ? 'viewer.html' : pathname.slice(1);
    if (['viewer.html', 'viewer.js', 'viewer.css', 'markdown.js', 'tool-output.js', 'manifest.webmanifest', 'icon.svg'].includes(file))
      return route.fulfill({ path: path.join(webRoot, file === 'icon.svg' ? '../icons/icon.svg' : file) });
    return route.fulfill({ status: 404, body: '' });
  });
  await page.goto('https://viewer.test/#workspace/test/new');
  await expect(page.locator('#new-page')).toBeVisible();
  return state;
}

async function refresh(page, state) {
  const previous = state.snapshots;
  state.snapshot.revision++;
  // The viewer reconciles a changed daemon by reconnecting and reloading the
  // whole snapshot; the changes stream itself carries deltas this fixture
  // does not model.
  await page.evaluate(() => window.dispatchEvent(new Event('online')));
  await expect.poll(() => state.snapshots).toBeGreaterThan(previous);
  // Wait for the render associated with the completed response, not just its request.
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
}

async function projectStep(page, target = 'container') {
  await page.locator('#new-next').click();
  await page.locator('#new-target').getByRole('radio', { name: new RegExp(`^${target}`) }).check();
  await page.locator('#new-next').click();
}

async function captureNewSessionState(output, page, label, state, extra = null) {
  const viewport = page.viewportSize();
  output.push(`=== ${label} (${viewport.width}x${viewport.height}) ===`);
  output.push((await page.locator('#new-page').isVisible())
    ? (await page.locator('#new-page').innerText()).trim()
    : (await page.locator('#app').innerText()).trim());
  const controls = await page.locator('#new-page button, #new-page input, #new-page select, #new-page textarea').evaluateAll(nodes => nodes.map(node => ({
    tag: node.tagName.toLowerCase(),
    id: node.id || null,
    label: node.getAttribute('aria-label') || node.innerText?.trim() || node.getAttribute('name') || null,
    value: 'value' in node ? node.value : null,
    checked: 'checked' in node ? node.checked : null,
    disabled: 'disabled' in node ? node.disabled : null,
    pressed: node.getAttribute('aria-pressed'),
  })));
  output.push(`controls: ${JSON.stringify(controls)}`);
  output.push(`subagent selector count: ${await page.locator('#new-subagents').count()}`);
  if (state.actions.length) output.push(`action: POST /api/actions ${JSON.stringify(state.actions)}`);
  if (state.preflights.length) output.push(`request: POST /api/preflight/new ${JSON.stringify(state.preflights)}`);
  if (state.resourceOptionRequests.length) output.push(`request: GET /api/targets/*/resource-options ${JSON.stringify(state.resourceOptionRequests)}`);
  if (state.completions.length) output.push(`request: POST /api/paths/complete ${JSON.stringify(state.completions)}`);
  if (state.discoveries.length) output.push(`request: POST /api/projects/discover ${JSON.stringify(state.discoveries)}`);
  if (state.creates.length) output.push(`request: POST /api/bundles ${JSON.stringify(state.creates)}`);
  output.push(`project creation requests: ${state.creates.length}`);
  if (extra !== null) output.push(`layout: ${JSON.stringify(extra)}`);
}

async function startHeldLaunch(page, state) {
  let release;
  state.holdLaunch = new Promise(resolve => { release = resolve; });
  await page.locator('#new-next').click();
  await expect.poll(() => state.actions.length).toBe(1);
  await expect(page.locator('#new-next')).toBeDisabled();
  return async () => {
    release();
    await expect.poll(() => state.launchResponses).toBe(1);
    await expect(page.locator('#new-page')).toBeHidden();
  };
}

async function withNewSessionPage(context, options, output, label, scenario) {
  const page = await context.newPage();
  const state = await mount(page, options);
  try {
    const result = await scenario(page, state);
    const afterCapture = typeof result === 'function' ? result : null;
    await captureNewSessionState(output, page, label, state, afterCapture ? null : result ?? null);
    if (afterCapture) await afterCapture();
  } finally {
    await page.close();
  }
}

async function captureWizardPath(output, page, state, label) {
  for (let index = 0; index < 5; index += 1) {
    const progress = await page.locator('#new-progress').innerText();
    if (progress.includes('Review')) {
      await expect(page.locator('#new-step')).not.toContainText('Checking project');
      await captureNewSessionState(output, page, `${label}: ${progress}`, state);
      return;
    }
    await captureNewSessionState(output, page, `${label}: ${progress}`, state);

    const current = await page.locator('#new-step').innerText();
    const projectChoice = page.getByRole('button', { name: 'existing', exact: true });
    if (current.includes('Choose a project') && await projectChoice.count()) {
      await projectChoice.click();
      if ((await page.locator('#new-progress').innerText()) === progress) await page.locator('#new-next').click();
    } else {
      await page.locator('#new-next').click();
    }
    await expect.poll(() => page.locator('#new-progress').innerText()).not.toBe(progress);
  }
  throw new Error(`New-session wizard did not reach Review for ${label}`);
}

test('golden_viewer_new_session', async ({ context }) => {
  const { assertGolden } = await import('./golden.mjs');
  const output = [];

  await withNewSessionPage(context, {}, output, 'container resource allocation and launch', async (page, state) => {
    await page.locator('#new-next').click();
    await captureNewSessionState(output, page, 'shared container defaults', state);
    await page.locator('#new-resource-cpus').fill('6');
    await page.locator('#new-resource-memory').fill('12.5');
    await page.locator('#new-next').click();
    await page.getByRole('button', { name: 'existing', exact: true }).click();
    await expect(page.locator('#new-step')).not.toContainText('Checking project');
    return startHeldLaunch(page, state);
  });

  const ec2Choices = [
    { kind: 'aws-ec2', instance_type: 'c7i.large', vcpus: 2, memory_bytes: 4294967296 },
    { kind: 'aws-ec2', instance_type: 'm7i.2xlarge', vcpus: 8, memory_bytes: 34359738368 },
  ];
  await withNewSessionPage(context, {
    extraTargets: [{ id: 'ec2', kind: 'aws-ec2', resource_allocation_kind: 'aws-ec2', requires_project_directory: false }],
    resourceOptions: { ec2: { options: ec2Choices, default_allocation: ec2Choices[1] } },
  }, output, 'EC2 resource choice and launch', async (page, state) => {
    await page.locator('#new-next').click();
    await page.locator('#new-target').getByRole('radio', { name: /^ec2/ }).check();
    await expect.poll(() => page.locator('#new-resource-aws').inputValue()).toBe('m7i.2xlarge');
    await captureNewSessionState(output, page, 'EC2 default resource option', state);
    await page.locator('#new-resource-aws').selectOption('c7i.large');
    await page.locator('#new-next').click();
    await page.getByRole('button', { name: 'existing', exact: true }).click();
    await expect(page.locator('#new-step')).not.toContainText('Checking project');
    return startHeldLaunch(page, state);
  });

  await withNewSessionPage(context, {}, output, 'host-specific path suggestions and URL source', async (page, state) => {
    await projectStep(page, 'local');
    const directory = page.locator('#new-project-directory');
    await directory.fill('/work/re');
    await expect.poll(() => state.completions.length).toBe(1);
    await captureNewSessionState(output, page, 'local path suggestions stay with the local host', state);
    await directory.press('ArrowDown');
    await directory.press('Enter');
    await expect.poll(() => state.completions.length).toBe(2);
    await captureNewSessionState(output, page, 'selected path suggestion and child completion', state);
    await page.locator('#new-back').click();
    await page.locator('#new-target').getByRole('radio', { name: /^container/ }).check();
    await page.locator('#new-next').click();
    await page.getByRole('button', { name: /^(Paste URL|URL)/ }).click();
    await page.locator('#new-project-source').fill('owner/repo');
    await expect(page.locator('#new-project-source')).toHaveValue('owner/repo');
  });

  await withNewSessionPage(context, {}, output, 'explicit managed worktree choice and launch', async (page, state) => {
    state.worktreeOptions = { available: true, default_create: false };
    await projectStep(page, 'local');
    await page.locator('#new-project-directory').fill('/work/linked');
    await expect.poll(() => state.completions.length).toBe(1);
    await page.locator('#new-next').click();
    await page.getByRole('checkbox', { name: 'Create isolated checkout' }).check();
    await expect(page.locator('#new-step')).not.toContainText('Checking project');
    await captureNewSessionState(output, page, 'managed worktree selected on Review', state);
    return startHeldLaunch(page, state);
  });

  for (const [policy, label] of [
    [undefined, 'Native'],
    [{ mode: 'none' }, 'None'],
    [{ mode: 'single_model', model: 'model-a', effort: 'high' }, 'Single model: model-a · high'],
    [{ mode: 'single_model', model: 'haiku', effort: 'low' }, 'Single model: haiku · low'],
    [{ mode: 'single_model', model: 'haiku', effort: null }, 'Single model: haiku'],
  ]) {
    await withNewSessionPage(context, {}, output, `profile subagent policy ${label}`, async (page, state) => {
      if (policy) {
        state.snapshot.profiles[0].subagents = policy;
        await refresh(page, state);
      }
      await projectStep(page, 'container');
      await page.getByRole('button', { name: 'existing', exact: true }).click();
      await expect(page.locator('#new-step')).not.toContainText('Checking project');
      await captureNewSessionState(output, page, `review subagent policy ${label}`, state);
      return startHeldLaunch(page, state);
    });
  }

  await withNewSessionPage(context, {}, output, 'harness without subagent support', async (page, state) => {
    await page.locator('#new-profile').selectOption('gamma');
    await projectStep(page, 'container');
    await page.getByRole('button', { name: 'existing', exact: true }).click();
    await expect(page.locator('#new-step')).not.toContainText('Checking project');
    await captureNewSessionState(output, page, 'review for harness without subagent support', state);
    return startHeldLaunch(page, state);
  });

  for (const target of ['container', 'local', 'remote']) {
    await withNewSessionPage(context, {}, output, `managed worktree unavailable on ${target}`, async (page, state) => {
      state.worktreeOptions = { available: false, default_create: false };
      await projectStep(page, target);
      if (target === 'container') await page.getByRole('button', { name: 'existing', exact: true }).click();
      else await page.locator('#new-next').click();
      await expect(page.locator('#new-step')).not.toContainText('Checking project');
    });
  }

  await withNewSessionPage(context, { bundles: [
    { id: 'existing', repositories: [{ id: 'frontend', github: 'example/frontend' }, { id: 'api', github: 'example/api' }] },
    { id: 'saved', repositories: [] },
  ] }, output, 'recent and saved multi-repository project selection', async (page, state) => {
    state.snapshot.sessions.push({ id: 'recent', workspace_id: 'test', bundle_id: 'existing', capabilities: {} });
    await refresh(page, state);
    await projectStep(page);
    await expect(page.locator('#new-step')).not.toContainText('Refreshing recent projects…');
    await captureNewSessionState(output, page, 'recent and saved project choices', state);
    await page.getByRole('button', { name: 'existing example/frontend · example/api', exact: true }).click();
    await expect(page.locator('#new-step')).not.toContainText('Checking project');
  });

  await withNewSessionPage(context, { bundles: [] }, output, 'empty and truncated GitHub results can continue by URL', async (page, state) => {
    state.discover = () => ({ entries: [], directory: null, parent: null, truncated: true });
    await projectStep(page);
    await page.getByRole('button', { name: /^GitHub/ }).click();
    await expect(page.locator('.project-browser')).toContainText('No repositories found');
    await captureNewSessionState(output, page, 'empty and truncated GitHub results', state);
    await page.getByRole('button', { name: /^(Paste URL|URL)/ }).click();
    await expect(page.locator('#new-project-source')).toBeFocused();
  });

  await withNewSessionPage(context, { bundles: [] }, output, 'folder filtering and opening a result without selecting it', async (page, state) => {
    state.discover = body => ({
      entries: body.filter === 'omitted'
        ? [{ name: 'omitted', source: '/home/controller/omitted', kind: 'repository' }]
        : [{ name: 'first', source: '/home/controller/first', kind: 'directory' }],
      directory: body.path || '/home/controller', parent: '/', truncated: !body.filter,
    });
    await projectStep(page);
    await page.getByRole('button', { name: /^(Browse folders|Folders)/ }).click();
    await page.getByRole('textbox', { name: 'Filter folder names' }).fill('omitted');
    await expect(page.getByRole('button', { name: /^omitted / })).toBeVisible();
    await captureNewSessionState(output, page, 'filtered folder result', state);
    await page.getByRole('button', { name: 'Open folder omitted', exact: true }).click();
    await expect(page.getByRole('button', { name: /^first / })).toBeVisible();
  });

  await withNewSessionPage(context, { bundles: [] }, output, 'actionable GitHub service failure', async (page, state) => {
    state.discover = () => ({ error: 'GitHub is temporarily unavailable. Retry later.' });
    await projectStep(page);
    await page.getByRole('button', { name: /^GitHub/ }).click();
    await expect(page.locator('.project-browser').getByRole('alert')).toHaveText('GitHub is temporarily unavailable. Retry later.');
  });

  await withNewSessionPage(context, { bundles: [] }, output, 'compact phone source navigation', async (page, state) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await projectStep(page);
    await page.getByRole('button', { name: /^Browse folders/ }).click();
    await expect(page.getByRole('button', { name: /^code Folder/ })).toBeVisible();
    const projectLayout = () => page.evaluate(() => {
      const rect = node => {
        const box = node.getBoundingClientRect();
        return { x: +box.x.toFixed(2), y: +box.y.toFixed(2), width: +box.width.toFixed(2), height: +box.height.toFixed(2) };
      };
      const sources = document.querySelector('[aria-label="Find a project"]');
      const buttons = [...(sources?.querySelectorAll('button') || [])];
      const result = [...document.querySelectorAll('button')].find(button => /^(code Folder|Use code)/.test(button.innerText.trim()));
      return {
        header: rect(document.querySelector('#shell-header')),
        sourceButtons: buttons.map(button => ({ text: button.innerText, ...rect(button) })),
        // A text-sized button's width follows the installed fonts; record
        // only that it stays inside the viewport.
        result: result ? (({ width, ...box }) => ({
          text: result.innerText,
          ...box,
          fitsViewport: result.getBoundingClientRect().right <= window.innerWidth,
        }))(rect(result)) : null,
        documentWidth: document.documentElement.scrollWidth,
      };
    });
    await captureNewSessionState(output, page, 'compact phone project chooser', state, await projectLayout());
    await page.getByRole('button', { name: /^code Folder/ }).click();
    await expect(page.getByRole('button', { name: /^Use code/ })).toBeVisible();
    await captureNewSessionState(output, page, 'opened folder remains visible on phone', state, await projectLayout());
    await page.getByRole('group', { name: 'Find a project' }).getByRole('button', { name: 'URL', exact: true }).click();
    await expect(page.locator('#new-project-source')).toBeVisible();
    await captureNewSessionState(output, page, 'phone URL source choice', state);
    await page.getByRole('button', { name: 'Recent & saved projects', exact: true }).click();
    await captureNewSessionState(output, page, 'phone recent projects source choice', state);
    const foldersButton = page.getByRole('button', { name: /^Browse folders/ });
    await expect(foldersButton).toContainText('On your Mjolnir computer');
    return foldersButton.boundingBox();
  });

  const unavailable = {
    error: 'Selected subagent model "removed" is unavailable.',
    code: 'subagent_choice_unavailable',
  };
  await withNewSessionPage(context, {}, output, 'unavailable subagent model and Settings guidance', async (page, state) => {
    state.snapshot.profiles[0].subagents = { mode: 'single_model', model: 'removed', effort: 'high' };
    state.actionError = unavailable;
    await refresh(page, state);
    await projectStep(page, 'container');
    await page.getByRole('button', { name: 'existing', exact: true }).click();
    await expect(page.locator('#new-step')).not.toContainText('Checking project');
    await captureNewSessionState(output, page, 'review shows unavailable subagent policy', state);
    await page.locator('#new-next').click();
    await expect(page.locator('#new-error')).toContainText('Settings → Agent Profiles → alpha → Sub-agents');
  });

  await withNewSessionPage(context, {}, output, 'generic launch error', async (page, state) => {
    state.actionError = { error: 'boom' };
    await projectStep(page, 'container');
    await page.getByRole('button', { name: 'existing', exact: true }).click();
    await page.locator('#new-next').click();
    await expect(page.locator('#new-error')).toHaveText('boom');
  });

  assertGolden('viewer_new_session', output.join('\n'));
});

test('golden_web_new_session_wizard_steps', async ({ context }) => {
  const { assertGolden } = await import('./golden.mjs');
  const output = [];
  const bare = { id: 'local', kind: 'local', requires_project_directory: true, recent_project_directories: ['/work/a'] };
  const sized = { ...defaultContainerTarget, id: 'container', kind: 'local-podman', requires_project_directory: false };
  const cases = [
    ['one profile omits Account but keeps Where to run', [{ id: 'only', harness_kind: 'codex' }], [bare, sized]],
    ['several profiles keep Account', [{ id: 'a', harness_kind: 'codex' }, { id: 'b', harness_kind: 'claude' }], [bare, sized]],
    ['only usable raw target omits Where to run', [{ id: 'a', harness_kind: 'codex' }, { id: 'b', harness_kind: 'claude' }], [bare, { ...sized, runtime_missing: true }, { id: 'mac', kind: 'ssh', requires_project_directory: true, availability: 'unavailable' }]],
    ['unknown target remains offered', [{ id: 'a', harness_kind: 'codex' }, { id: 'b', harness_kind: 'claude' }], [bare, { ...sized, availability: 'unknown' }]],
    ['single sized target keeps Where to run', [{ id: 'a', harness_kind: 'codex' }, { id: 'b', harness_kind: 'claude' }], [sized]],
  ];
  for (const [label, profiles, targets] of cases) {
    const page = await context.newPage();
    const state = await mount(page, { profiles, targets, bundles: [{ id: 'existing', repositories: [] }] });
    try {
      await captureWizardPath(output, page, state, label);
    } finally {
      await page.close();
    }
  }

  const skippedPage = await context.newPage();
  const skippedState = await mount(skippedPage, {
    profiles: [{ id: 'fake', harness_kind: 'codex' }],
    targets: [bare],
    bundles: [{ id: 'existing', repositories: [] }],
  });
  try {
    await skippedPage.locator('#new-next').click();
    await expect(skippedPage.locator('#new-step')).not.toContainText('Checking project');
    await captureNewSessionState(output, skippedPage, 'review after skipped steps supplies all required values', skippedState);
    const finishLaunch = await startHeldLaunch(skippedPage, skippedState);
    await captureNewSessionState(output, skippedPage, 'launch payload after skipped steps', skippedState);
    await finishLaunch();
  } finally {
    await skippedPage.close();
  }
  assertGolden('web_new_session_wizard_steps', output.join('\n'));
});

test('container remembered size is clamped to the reported host limits', async ({ page }) => {
  const containerTarget = {
    ...defaultContainerTarget,
    remembered_container_size: { cpus: 14, memory_bytes: 51539607552 },
    container_host_limits: { cpus: 10, memory_bytes: 25769803776 },
    default_resource_allocation: { kind: 'container', cpus: 10, memory_bytes: 25769803776 },
  };
  await mount(page, { containerTarget });
  await page.locator('#new-next').click();
  await expect(page.locator('#new-resource-cpus')).toHaveValue('10');
  await expect(page.locator('#new-resource-memory')).toHaveValue('24');
});

test('whole-row taps and in-progress gestures survive unrelated live updates', async ({ page }) => {
  const state = await mount(page);
  // The Account step is a native select: the choice is made there, and both
  // the element and its value survive an unrelated live update.
  const profile = page.locator('#new-profile');
  const originalProfile = await profile.elementHandle();
  await profile.selectOption('beta');
  state.snapshot.profiles[0].quota = { summary: 'new live reading' };
  await refresh(page, state);
  expect(await originalProfile.evaluate(node => node.isConnected)).toBe(true);
  await expect(profile).toHaveValue('beta');
  await refresh(page, state);
  await expect(profile).toHaveValue('beta');

  // The Where-to-run step keeps radio rows; a whole-row tap is one target,
  // and an in-progress gesture survives an unrelated live update.
  await page.locator('#new-next').click();
  const local = page.locator('#new-target').getByRole('radio', { name: /^local/ });
  const row = local.locator('..');
  const original = await local.elementHandle();
  const box = await row.boundingBox();
  expect(box.height).toBeGreaterThanOrEqual(44);
  await page.mouse.move(box.x + box.width - 8, box.y + box.height / 2);
  await page.mouse.down();
  state.snapshot.profiles[0].quota = { summary: 'another live reading' };
  await refresh(page, state);
  expect(await original.evaluate(node => node.isConnected)).toBe(true);
  await page.mouse.up();
  await expect(local).toBeChecked();
  await refresh(page, state);
  await expect(local).toBeChecked();

  await page.locator('#new-next').click();
  const directory = page.locator('#new-project-directory');
  await directory.fill('/work/typed');
  await refresh(page, state);
  await expect(directory).toBeFocused();
  await expect(directory).toHaveValue('/work/typed');
});

test('raw projects use host-specific recents and preserve edited paths across Back', async ({ page }) => {
  const state = await mount(page);
  await projectStep(page, 'local');
  const directory = page.locator('#new-project-directory');
  await expect(directory).toHaveValue('/work/recent');
  await page.getByRole('button', { name: '/work/older', exact: true }).tap();
  await expect(directory).toHaveValue('/work/older');
  await directory.fill('/work/custom');
  await page.locator('#new-back').click();
  await page.locator('#new-target').getByRole('radio', { name: /^remote/ }).check();
  await page.locator('#new-next').click();
  await expect(directory).toHaveValue('/remote/recent');
  await expect(page.getByRole('button', { name: '/work/older', exact: true })).toHaveCount(0);
  await page.locator('#new-back').click();
  await page.locator('#new-target').getByRole('radio', { name: /^local/ }).check();
  await page.locator('#new-next').click();
  await expect(directory).toHaveValue('/work/custom');
  await directory.fill('');
  await page.locator('#new-next').click();
  await expect(page.locator('#new-error')).toContainText('Name the project directory');
  expect(state.preflights).toHaveLength(0);
  await directory.fill('/work/custom');
  await page.locator('#new-next').click();
  await expect(page.locator('#new-step')).toContainText('/work/custom');
  expect(state.preflights[0]).toMatchObject({ target_id: 'local', project_directory: '/work/custom' });
});

// Suggestions answer the machine that owns the path and never rewrite what
// is being typed; pasting a URL never searches a filesystem.

// The project list finishing re-renders the step and replaces the field. A
// slow machine makes that land after the person has typed.
// Hard-won: eef1275: a project-step redraw dropped the current path suggestion reply on slow machines.
test('path suggestions survive the step re-rendering while a reply is pending', async ({ page }) => {
  const state = await mount(page);
  let finishCatalog;
  state.holdCatalog = new Promise(resolve => { finishCatalog = resolve; });
  let answer;
  const held = new Promise(resolve => { answer = resolve; });
  state.complete = async () => { await held; return { candidates: ['/work/recent/', '/work/repos/'], insert: '/work/re', truncated: false }; };
  await projectStep(page, 'local');
  const directory = page.locator('#new-project-directory');
  await directory.fill('/work/re');
  await expect.poll(() => state.completions.length).toBe(1);
  finishCatalog();
  await expect(page.locator('#new-step')).not.toContainText('Refreshing recent projects');
  answer();
  await expect(page.locator('.field-suggestions .palette-row[role="option"]')).toHaveCount(2);
  await expect(page.locator('#new-project-directory')).toHaveValue('/work/re');
  await expect(page.locator('#new-project-directory')).toBeFocused();

  // A reply for an older value never shows.
  state.completions.length = 0;
  let late;
  state.complete = async body => { if (body.prefix === '/work/rep') await new Promise(resolve => { late = resolve; }); return { candidates: ['/old/'], insert: '', truncated: false }; };
  await page.locator('#new-project-directory').pressSequentially('p');
  await expect.poll(() => state.completions.length).toBe(1);
  await page.locator('#new-project-directory').pressSequentially('o');
  late();
  await page.waitForTimeout(300);
  await expect(page.locator('.field-suggestions .palette-row[role="option"]')).toHaveCount(1);
  await expect(page.locator('.field-suggestions .palette-row[role="option"]')).toHaveText('/old/');
});

test('empty projects show choices and a pasted source retries then continues directly to review', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  await projectStep(page);
  await expect(page.locator('#new-next')).toBeHidden();
  await expect(page.locator('#new-title')).toHaveCount(0);
  await expect(page.locator('#new-project-source')).toHaveCount(0);
  await expect(page.locator('#new-step')).not.toContainText(/bundle/i);
  await page.getByRole('button', { name: /^(Paste URL|URL)/ }).click();
  await page.locator('#new-project-source').fill('example/created');
  state.rejectCreate = true;
  await page.getByRole('button', { name: 'Use repository', exact: true }).click();
  await expect(page.locator('#new-error')).toContainText('Repository source is invalid');
  await expect(page.locator('#new-project-source')).toHaveValue('example/created');
  state.rejectCreate = false;
  await page.locator('#new-project-source').press('Enter');
  await expect(page.locator('#new-progress')).toContainText('Review');
  await expect(page.locator('#new-step')).toContainText('Project');
  await expect(page.locator('#new-step')).not.toContainText(/bundle/i);
  expect(state.creates).toEqual([{ sources: ['example/created'] }, { sources: ['example/created'] }]);
  await expect(page.locator('#new-step')).toContainText('Local changes');
  await page.locator('#new-next').click();
  await expect(page).toHaveURL(/#workspace\/test$/);
  expect(state.actions).toHaveLength(1);
  expect(state.actions[0]).toMatchObject({ action: 'new', workspace_id: 'test', bundle_id: 'created' });
  expect(state.actions[0]).not.toHaveProperty('dirty_ack');
});

test('late launch completion cannot replace another workspace wizard', async ({ page }) => {
  const state = await mount(page);
  await projectStep(page);
  await page.getByRole('button', { name: 'existing', exact: true }).click();
  let release;
  state.holdLaunch = new Promise(resolve => { release = resolve; });
  await page.locator('#new-next').click();
  await expect.poll(() => state.actions.length).toBe(1);
  await expect(page.locator('#new-next')).toBeDisabled();
  await page.evaluate(() => { location.hash = '#workspace/other/new'; });
  await expect(page.locator('#new-profile')).toBeVisible();
  release();
  await refresh(page, state);
  await expect(page).toHaveURL(/#workspace\/other\/new$/);
  await expect(page.locator('#new-profile')).toBeVisible();
  expect(state.actions[0].workspace_id).toBe('test');
});

test('leaving a new wizard aborts its stale preflight request', async ({ page }) => {
  const state = await mount(page);
  await projectStep(page);
  let release;
  state.holdPreflight = new Promise(resolve => { release = resolve; });
  await page.getByRole('button', { name: 'existing', exact: true }).click();
  await expect.poll(() => state.preflights.length).toBe(1);
  await expect(page.locator('#new-next')).toHaveText('Checking…');
  await page.evaluate(() => { location.hash = '#workspace/other/new'; });
  await expect(page.locator('#new-profile')).toBeVisible();
  release();
  await expect.poll(() => state.preflightFailures).toBeGreaterThan(0);
  await expect(page.locator('#new-step')).toContainText('Account');
  await expect(page.locator('#new-step')).not.toContainText('Local changes');
});

test('Review is usable during preflight, survives refresh, and gates submission', async ({ page }) => {
  const state = await mount(page);
  await projectStep(page, 'local');
  let release;
  state.holdPreflight = new Promise(resolve => { release = resolve; });
  state.resolvedDirectory = '/resolved/project';
  await page.locator('#new-next').click();
  await expect(page.locator('#new-progress')).toContainText('Review');
  await expect(page.locator('#new-step')).toContainText('Checking project…');
  await expect(page.locator('#new-next')).toBeDisabled();
  await expect(page.locator('#new-back')).toBeEnabled();
  const worktree = page.getByRole('checkbox', { name: 'Create isolated checkout' });
  await expect(worktree).toBeDisabled();
  await expect(page.locator('#new-step')).toContainText('SubagentsNative');
  await page.locator('#new-form').evaluate(form => form.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })));
  expect(state.actions).toHaveLength(0);
  await refresh(page, state);
  await expect(page.locator('#new-step')).toContainText('SubagentsNative');
  await expect(page.locator('#new-step')).toContainText('Checking project…');
  expect(state.preflights).toHaveLength(1);
  release();
  await expect(page.locator('#new-next')).toHaveText('Start');
  await expect(page.locator('#new-next')).toBeEnabled();
  await expect(worktree).toBeChecked();
  await expect(page.locator('#new-step')).toContainText('/resolved/project');
  await page.locator('#new-next').click();
  await expect.poll(() => state.actions.length).toBe(1);
  expect(state.actions[0]).toMatchObject({ project_directory: '/resolved/project', subagents: { mode: 'native' } });
});

test('failed Review retries without launching and Back abandons a pending check', async ({ page }) => {
  const state = await mount(page);
  await projectStep(page);
  state.preflightError = 'Remote unavailable';
  await page.getByRole('button', { name: 'existing', exact: true }).click();
  await expect(page.locator('#new-progress')).toContainText('Review');
  await expect(page.locator('#new-step')).toContainText('Remote unavailable');
  await expect(page.locator('#new-next')).toHaveText('Retry');
  state.preflightError = null;
  let release;
  state.holdPreflight = new Promise(resolve => { release = resolve; });
  await page.locator('#new-next').click();
  await expect.poll(() => state.preflights.length).toBe(2);
  expect(state.actions).toHaveLength(0);
  await page.locator('#new-back').click();
  await expect(page.locator('#new-progress')).toContainText('Project');
  state.holdPreflight = null;
  await page.getByRole('button', { name: 'existing', exact: true }).click();
  await expect(page.locator('#new-next')).toHaveText('Start');
  await expect.poll(() => state.preflights.length).toBe(3);
  release();
  await expect.poll(() => state.preflightFailures).toBeGreaterThan(0);
  await expect(page.locator('#new-progress')).toContainText('Review');
  await expect(page.locator('#new-next')).toBeEnabled();
  expect(state.actions).toHaveLength(0);
});

test('declining repair keeps Review unready and allows a fresh retry', async ({ page }) => {
  const state = await mount(page);
  state.remoteRepairs = [{ path: '/project', branch: 'main', missing_remote: 'old', replacement_remote: 'origin', fetch_url: 'https://example.com/project.git', push_urls: [] }];
  page.on('dialog', dialog => dialog.dismiss());
  await projectStep(page);
  await page.getByRole('button', { name: 'existing', exact: true }).click();
  await expect(page.locator('#new-step')).toContainText('repair was declined');
  await expect(page.locator('#new-next')).toHaveText('Retry');
  expect(state.preflights).toHaveLength(1);
  expect(state.actions).toHaveLength(0);
  state.remoteRepairs = [];
  await page.locator('#new-next').click();
  await expect(page.locator('#new-next')).toHaveText('Start');
  expect(state.preflights).toHaveLength(2);
  expect(state.actions).toHaveLength(0);
});

test('a pending client does not prevent another client from completing preflight', async ({ page, context }) => {
  const first = await mount(page);
  await projectStep(page);
  let release;
  first.holdPreflight = new Promise(resolve => { release = resolve; });
  await page.getByRole('button', { name: 'existing', exact: true }).click();
  await expect(page.locator('#new-next')).toBeDisabled();
  const other = await context.newPage();
  const second = await mount(other);
  await projectStep(other);
  await other.getByRole('button', { name: 'existing', exact: true }).click();
  await expect(other.locator('#new-next')).toHaveText('Start');
  await expect(other.locator('#new-next')).toBeEnabled();
  expect(second.preflights).toHaveLength(1);
  await expect(page.locator('#new-next')).toBeDisabled();
  release();
  await expect(page.locator('#new-next')).toBeEnabled();
});

test('project creation stays single-flight and a late result cannot alter a replacement wizard', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  await projectStep(page);
  let release;
  state.holdCreate = new Promise(resolve => { release = resolve; });
  await page.getByRole('button', { name: /^(Paste URL|URL)/ }).click();
  await page.locator('#new-project-source').fill('example/created');
  await page.getByRole('button', { name: 'Use repository', exact: true }).click();
  await expect.poll(() => state.creates.length).toBe(1);
  await expect(page.getByRole('button', { name: 'Opening project…', exact: true })).toBeDisabled();
  await expect(page.locator('#new-next')).toBeDisabled();
  await page.evaluate(() => { location.hash = '#workspace/other/new'; });
  await expect(page.locator('#new-profile')).toBeVisible();
  release();
  await refresh(page, state);
  await expect(page).toHaveURL(/#workspace\/other\/new$/);
  await expect(page.locator('#new-profile')).toBeVisible();
  expect(state.creates).toHaveLength(1);
});

test('resume choices stay selected through revisions and are used by Resume', async ({ page }) => {
  const state = await mount(page);
  state.snapshot.sessions.push({
    id: 'suspended', title: 'Suspended test', state: 'suspended', lifecycle: 'suspended',
    workspace_id: 'test', profile_id: 'alpha', target_id: 'local',
    capabilities: { resume: true }, compatible_resume_targets: ['local', 'remote'],
    queued_prompts: [{ id: 'queued', text: 'queued work' }],
  });
  await refresh(page, state);
  await page.evaluate(() => { location.hash = '#workspace/test/resume'; });
  await expect(page.locator('#resume-list-view')).toBeVisible();
  await expect(page.locator('#resume-detail-view')).toBeHidden();
  await page.locator('#resumable [data-session-id="suspended"]').click();
  await expect(page).toHaveURL(/\/resume\/suspended$/);
  const detail = page.locator('#resume-detail');
  await detail.locator('[data-role="resume-profile"] select').selectOption('beta');
  await detail.locator('[data-role="resume-target"] select').selectOption('remote');
  await detail.locator('[data-role="resume-queue"] select').selectOption('discard');
  await refresh(page, state);
  await expect(detail.locator('[data-role="resume-profile"] select')).toHaveValue('beta');
  await expect(detail.locator('[data-role="resume-queue"] select')).toHaveValue('discard');
  await detail.getByRole('button', { name: 'Resume', exact: true }).click();
  await expect.poll(() => state.actions.length).toBe(1);
  await expect(page).toHaveURL(/#workspace\/test$/);
  expect(state.actions[0]).toEqual({ action: 'resume', session_id: 'suspended', workspace_id: 'test', profile_id: 'beta', target_id: 'remote', queue: 'discard' });
});

test('managed worktree defaults can be overridden and survive Back and live refresh', async ({ page }) => {
  const state = await mount(page);
  await projectStep(page, 'local');
  await page.locator('#new-next').click();
  const checkbox = page.getByRole('checkbox', { name: 'Create isolated checkout' });
  await expect(checkbox).toBeChecked();
  await checkbox.uncheck();
  await expect(page.locator("#new-step")).toContainText("Use the selected directory directly.");
  await refresh(page, state);
  await expect(checkbox).not.toBeChecked();
  await page.locator('#new-back').click();
  await page.locator('#new-next').click();
  await expect(checkbox).not.toBeChecked();
  await expect(page.locator('#new-step')).toContainText('Use the selected directory directly.');
  await page.locator('#new-next').click();
  expect(state.actions.at(-1)).toMatchObject({ create_managed_worktree: false, project_directory: '/work/recent' });
});

test('changing the directory resets the worktree choice to its inspected default', async ({ page }) => {
  const state = await mount(page);
  await projectStep(page, 'local');
  await page.locator('#new-next').click();
  const checkbox = page.getByRole('checkbox', { name: 'Create isolated checkout' });
  await checkbox.uncheck();
  await page.locator('#new-back').click();
  await page.locator('#new-project-directory').fill('/work/another');
  await page.locator('#new-next').click();
  await expect(checkbox).toBeChecked();
});

test('GitHub lists accessible repositories, preserves focus across snapshots and searches with the keyboard', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  await projectStep(page);
  await page.getByRole('button', { name: /^GitHub/ }).tap();
  await expect(page.getByRole('button', { name: /^example\/app/ })).toBeVisible();
  expect(state.discoveries).toEqual([{ kind: 'github', query: '' }]);
  const search = page.getByRole('textbox', { name: 'Search GitHub repositories' });
  await search.fill('example/api');
  await expect.poll(() => state.discoveries.length).toBe(2);
  expect(state.discoveries[1]).toEqual({ kind: 'github', query: 'example/api' });
  await expect(search).toBeFocused();
  await expect(page.getByRole('button', { name: /^example\/app/ })).toBeVisible();
  const row = page.getByRole('button', { name: /^example\/app/ });
  const original = await row.elementHandle();
  state.snapshot.bundles.push({ id: 'another-clients-project', repositories: [] });
  await refresh(page, state);
  await expect(search).toBeFocused();
  expect(await original.evaluate(node => node.isConnected)).toBe(true);
  await page.getByRole('button', { name: 'Clear search', exact: true }).click();
  await expect(search).toHaveValue('');
  await expect(search).toBeFocused();
  await expect.poll(() => state.discoveries.length).toBe(3);
  await row.focus();
  await page.keyboard.press('Enter');
  await expect(page.locator('#new-progress')).toContainText('Review');
  expect(state.creates).toEqual([{ sources: ['https://github.com/example/app'] }]);
  expect(state.preflights[0].bundle_id).toBe('created');
});

test('GitHub authentication errors keep their sign-in instructions and folders remain usable', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  const discover = state.discover;
  state.discover = body => body.kind === 'github' ? { error: 'GitHub authentication required. Sign in on the computer running Mjolnir, then retry.' } : discover(body);
  await projectStep(page);
  await page.getByRole('button', { name: /^GitHub/ }).click();
  await expect(page.locator('.project-browser').getByRole('alert')).toContainText('GitHub authentication required');
  await expect(page.locator('.project-browser')).toContainText('Sign in on the computer running Mjolnir');
  await page.getByRole('button', { name: 'Retry', exact: true }).click();
  await expect.poll(() => state.discoveries.length).toBe(2);
  await page.getByRole('button', { name: /^(Browse folders|Folders)/ }).tap();
  await expect(page.locator('.project-browser')).toContainText('Folders on the computer running Mjolnir.');
  await expect(page.getByRole('button', { name: /^code Folder/ })).toBeVisible();
  expect(state.discoveries.at(-1)).toEqual({ kind: 'directory', path: '' });
  await page.getByRole('button', { name: /^code Folder/ }).tap();
  await expect(page.getByRole('button', { name: /^Use code/ })).toBeVisible();
  await page.getByRole('button', { name: 'Up', exact: true }).click();
  await expect(page.locator('#new-project-current-folder')).toHaveText('/home/controller');
  await page.getByRole('button', { name: /^code Folder/ }).click();
  await page.getByRole('button', { name: /^Use code/ }).click();
  await expect(page.locator('#new-progress')).toContainText('Review');
  expect(state.creates).toEqual([{ sources: ['/home/controller/code'] }]);
  expect(state.completions).toHaveLength(0);
});

test('superseded discovery and cancelled wizard requests cannot insert old results', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  const discover = state.discover;
  let release;
  state.discover = body => body.kind === 'github' ? new Promise(resolve => {
    release = () => resolve(discover(body));
  }) : discover(body);
  await projectStep(page);
  await page.getByRole('button', { name: /^GitHub/ }).click();
  await expect(page.locator('.project-browser').getByRole('status')).toContainText('Loading repositories');
  await page.getByRole('button', { name: /^(Browse folders|Folders)/ }).click();
  await expect(page.getByRole('button', { name: /^code Folder/ })).toBeVisible();
  release();
  await expect.poll(() => state.discoveryFailures).toBe(1);
  await expect(page.getByRole('button', { name: /^example\/app/ })).toHaveCount(0);
  await page.getByRole('button', { name: /^GitHub/ }).click();
  await expect(page.locator('.project-browser').getByRole('status')).toContainText('Loading repositories');
  await page.evaluate(() => { location.hash = '#workspace/other/new'; });
  await expect(page.locator('#new-profile')).toBeVisible();
  release();
  await expect.poll(() => state.discoveryFailures).toBe(2);
  await projectStep(page);
  await expect(page.getByRole('heading', { name: 'Choose a project' })).toBeVisible();
  await expect(page.locator('.project-browser')).toHaveCount(0);
});

test('remote folder browsing stays on the selected host and opens its current folder', async ({ page }) => {
  const state = await mount(page);
  state.complete = body => ({ candidates: [`${body.prefix}child/`], truncated: false });
  await projectStep(page, 'remote');
  await page.getByRole('button', { name: 'Browse folders', exact: true }).click();
  await expect(page.locator('.project-browser')).toContainText('Browsing folders on remote');
  await page.getByRole('button', { name: /^\/remote\/recent\/child\// }).click();
  await expect(page.locator('#new-project-current-folder')).toHaveText('/remote/recent/child/');
  await page.getByRole('button', { name: 'Up', exact: true }).click();
  await expect(page.locator('#new-project-current-folder')).toHaveText('/remote/recent/');
  await page.getByRole('button', { name: 'Home', exact: true }).click();
  await expect(page.locator('#new-project-current-folder')).toHaveText('~/');
  await page.getByRole('button', { name: 'Use this folder', exact: true }).click();
  await expect(page.locator('#new-progress')).toContainText('Review');
  expect(state.completions).toEqual([
    { target_id: 'remote', prefix: '/remote/recent/', kind: 'directories' },
    { target_id: 'remote', prefix: '/remote/recent/child/', kind: 'directories' },
    { target_id: 'remote', prefix: '/remote/recent/', kind: 'directories' },
    { target_id: 'remote', prefix: '~/', kind: 'directories' },
  ]);
  expect(state.discoveries).toHaveLength(0);
  expect(state.creates).toHaveLength(0);
  expect(state.preflights[0]).toMatchObject({ target_id: 'remote', project_directory: '~/' });
});

test('a discovery failure can retry and a phone chooser stays within the viewport', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  const discover = state.discover;
  state.discover = () => ({ error: 'Folder is unavailable' });
  await page.setViewportSize({ width: 320, height: 700 });
  await projectStep(page);
  await page.getByRole('button', { name: /^(Browse folders|Folders)/ }).tap();
  await expect(page.locator('.project-browser').getByRole('alert')).toContainText('Folder is unavailable');
  state.discover = discover;
  await page.getByRole('button', { name: 'Retry', exact: true }).tap();
  const folder = page.getByRole('button', { name: /^code Folder/ });
  await expect(folder).toBeVisible();
  expect((await folder.boundingBox()).height).toBeGreaterThanOrEqual(44);
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(320);
  await page.locator('#new-project-location-toggle').click();
  await page.getByRole('textbox', { name: 'Folder path' }).fill('/home/controller/code');
  await page.getByRole('textbox', { name: 'Folder path' }).press('Enter');
  await expect(page.getByRole('button', { name: /^Use code/ })).toBeVisible();
});

test('search, folder location, and pasted text survive switching source choices', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  await projectStep(page);
  await page.getByRole('button', { name: /^(Paste URL|URL)/ }).click();
  await page.locator('#new-project-source').fill('example/draft');
  await page.getByRole('button', { name: /^GitHub/ }).click();
  await page.getByRole('textbox', { name: 'Search GitHub repositories' }).fill('keep this query');
  await expect.poll(() => state.discoveries.length).toBe(2);
  await expect(page.getByRole('button', { name: /^example\/app/ })).toBeVisible();
  await page.getByRole('button', { name: /^(Browse folders|Folders)/ }).click();
  await page.getByRole('button', { name: /^code Folder/ }).click();
  await expect(page.getByRole('button', { name: /^Use code/ })).toBeVisible();
  await page.getByRole('button', { name: /^GitHub/ }).click();
  await expect(page.getByRole('textbox', { name: 'Search GitHub repositories' })).toHaveValue('keep this query');
  await page.getByRole('button', { name: /^(Paste URL|URL)/ }).click();
  await expect(page.locator('#new-project-source')).toHaveValue('example/draft');
  await page.getByRole('button', { name: /^(Browse folders|Folders)/ }).click();
  await expect(page.locator('#new-project-current-folder')).toHaveText('/home/controller/code');
  expect(state.discoveries).toHaveLength(4);
  expect(state.creates).toHaveLength(0);
});

test('cancelled folder loading offers a retry without leaving the chooser', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  const discover = state.discover;
  let release;
  state.discover = body => new Promise(resolve => { release = () => resolve(discover(body)); });
  await projectStep(page);
  await page.getByRole('button', { name: /^(Browse folders|Folders)/ }).click();
  await page.getByRole('button', { name: 'Cancel loading', exact: true }).click();
  await expect(page.locator('.project-browser')).toContainText('Loading cancelled');
  release();
  state.discover = discover;
  await page.getByRole('button', { name: 'Retry', exact: true }).click();
  await expect(page.getByRole('button', { name: /^code Folder/ })).toBeVisible();
  expect(state.creates).toHaveLength(0);
});

test('typing a folder path cancels its pending listing without replacing the edited path', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  const discover = state.discover;
  let release;
  state.discover = body => new Promise(resolve => { release = () => resolve(discover(body)); });
  await projectStep(page);
  await page.getByRole('button', { name: /^(Browse folders|Folders)/ }).click();
  await page.locator('#new-project-location-toggle').click();
  const location = page.getByRole('textbox', { name: 'Folder path' });
  await location.fill('/home/controller/code');
  release();
  await expect.poll(() => state.discoveryFailures).toBe(1);
  await expect(location).toHaveValue('/home/controller/code');
  await expect(location).toBeFocused();
  await expect(page.locator('.project-browser')).toContainText('Choose Go');
  await expect(page.locator('.project-result-row')).toHaveCount(0);
  state.discover = discover;
  await location.press('Enter');
  await expect(page.getByRole('button', { name: /^Use code/ })).toBeVisible();
});

test('managed projects require an explicit choice and Title can be edited during Review preflight', async ({ page }) => {
  const state = await mount(page, { bundles: [
    { id: 'first', repositories: [] }, { id: 'chosen', repositories: [] },
  ] });
  await projectStep(page);
  await expect(page.locator('#new-next')).toBeHidden();
  await expect(page.locator('#new-title')).toHaveCount(0);
  await expect(page.locator('#new-step')).not.toContainText('unpublished');
  await page.locator('#new-form').evaluate(form => form.requestSubmit());
  expect(state.preflights).toHaveLength(0);
  await expect(page.locator('#new-progress')).toContainText('Project');

  let release;
  state.holdPreflight = new Promise(resolve => { release = resolve; });
  await page.getByRole('button', { name: 'chosen', exact: true }).click();
  const title = page.getByRole('textbox', { name: 'Title (optional)', exact: true });
  await expect(title).toHaveAttribute('placeholder', 'chosen via alpha');
  await title.fill('Investigate startup');
  await title.press('Enter');
  expect(state.actions).toHaveLength(0);
  await refresh(page, state);
  await expect(title).toBeFocused();
  await expect(title).toHaveValue('Investigate startup');
  release();
  await expect(page.locator('#new-next')).toBeEnabled();
  await expect(title).toBeFocused();
  await expect(title).toHaveValue('Investigate startup');
  await page.locator('#new-back').click();
  await expect(page.locator('#new-title')).toHaveCount(0);
  await expect(page.locator('#new-next')).toBeHidden();
  await page.getByRole('button', { name: 'chosen', exact: true }).click();
  await expect(title).toHaveValue('Investigate startup');
  await expect(page.locator('#new-next')).toBeEnabled();
  await title.press('Enter');
  expect(state.actions[0]).toMatchObject({ bundle_id: 'chosen', title: 'Investigate startup' });
});

test('folder path disclosure is optional, keyboard accessible, and survives updates while editing', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  await projectStep(page);
  await page.getByRole('button', { name: /^(Browse folders|Folders)/ }).click();
  await expect(page.locator('#new-project-current-folder')).toHaveText('/home/controller');
  await expect(page.locator('#new-project-location')).toBeHidden();
  await expect(page.getByRole('button', { name: 'Home', exact: true })).toBeVisible();
  const disclosure = page.locator('#new-project-location-toggle');
  await disclosure.focus();
  await disclosure.press('Enter');
  const location = page.getByRole('textbox', { name: 'Folder path' });
  await location.fill('/home/controller/code');
  await refresh(page, state);
  await expect(location).toBeFocused();
  await expect(location).toHaveValue('/home/controller/code');
  await location.press('Enter');
  await expect(page.getByRole('button', { name: /^Use code/ })).toBeVisible();
  await expect(page.locator('#new-project-location')).toBeHidden();
  await expect(page.locator('#new-project-current-folder')).toBeFocused();
  expect(state.creates).toHaveLength(0);
});

test('opening and immediately editing a folder path preserves disclosure through redraw', async ({ page }) => {
  await mount(page, { bundles: [] });
  await projectStep(page);
  await page.getByRole('button', { name: /^Browse folders/ }).click();
  await expect(page.getByRole('button', { name: /^code Folder/ })).toBeVisible();
  // Toggle events are deferred. Edit in the same task to exercise the race.
  await page.evaluate(() => {
    document.querySelector('#new-project-location-toggle').click();
    const input = document.querySelector('#new-project-location');
    input.focus();
    input.value = '/home/controller/code';
    input.dispatchEvent(new Event('input', { bubbles: true }));
  });
  const location = page.getByRole('textbox', { name: 'Folder path' });
  await expect(location).toBeVisible();
  await expect(location).toBeFocused();
  await expect(location).toHaveValue('/home/controller/code');
  await location.press('Enter');
  await expect(page.getByRole('button', { name: /^Use code/ })).toBeVisible();
});

test('multiple repositories from GitHub, folders and URL retain order through removal, retry and Back', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  await projectStep(page);
  await page.getByRole('button', { name: 'Select multiple repositories', exact: true }).click();
  await page.getByRole('button', { name: /^GitHub/ }).click();
  await page.getByRole('button', { name: /^example\/app/ }).click();
  const repositories = page.getByRole('list', { name: 'Selected repositories' });
  await expect(repositories).toContainText('https://github.com/example/app · Primary');
  await page.getByRole('button', { name: /^example\/app/ }).click();
  await expect(page.locator('#new-error')).toContainText('already in the project');
  await expect(repositories.getByRole('listitem')).toHaveCount(1);
  await page.getByRole('button', { name: 'Folders', exact: true }).click();
  await page.getByRole('button', { name: /^code Folder/ }).click();
  await page.getByRole('button', { name: /^Use code/ }).click();
  await page.getByRole('button', { name: 'URL', exact: true }).click();
  await page.locator('#new-project-source').fill('example/shared');
  await page.locator('#new-project-source').press('Enter');
  await expect(repositories.getByRole('listitem')).toHaveCount(3);
  await expect(page.locator('#new-project-source')).toHaveValue('');
  await page.getByRole('button', { name: 'Remove https://github.com/example/app', exact: true }).click();
  await expect(repositories.getByRole('listitem').first()).toContainText('/home/controller/code · Primary');
  await refresh(page, state);
  await page.locator('#new-back').click();
  await page.locator('#new-next').click();
  await expect(repositories.getByRole('listitem')).toHaveCount(2);
  expect(state.creates).toHaveLength(0);
  state.rejectCreate = true;
  await page.getByRole('button', { name: 'Use project', exact: true }).click();
  await expect(page.locator('#new-error')).toContainText('Repository source is invalid');
  await expect(repositories.getByRole('listitem')).toHaveCount(2);
  state.rejectCreate = false;
  await page.getByRole('button', { name: 'Use project', exact: true }).click();
  await expect(page.locator('#new-progress')).toContainText('Review');
  expect(state.creates).toEqual([
    { sources: ['/home/controller/code', 'example/shared'] },
    { sources: ['/home/controller/code', 'example/shared'] },
  ]);
  await page.locator('#new-next').click();
  expect(state.actions[0]).toMatchObject({ bundle_id: 'created', target_id: 'container' });
});

test('group creation is single flight and leaving it preserves the draft while ignoring a late result', async ({ page }) => {
  const state = await mount(page, { bundles: [] });
  await projectStep(page);
  await page.getByRole('button', { name: 'Select multiple repositories', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Use project', exact: true })).toBeDisabled();
  await page.getByRole('button', { name: /^Paste URL/ }).click();
  for (const source of ['example/app', 'example/shared']) {
    await page.locator('#new-project-source').fill(source);
    await page.getByRole('button', { name: 'Add repository', exact: true }).click();
  }
  let release;
  state.holdCreate = new Promise(resolve => { release = resolve; });
  await page.getByRole('button', { name: 'Use project', exact: true }).click();
  await expect.poll(() => state.creates.length).toBe(1);
  await expect(page.getByRole('button', { name: 'Opening project…', exact: true })).toBeDisabled();
  await expect(page.getByRole('button', { name: 'Remove example/app', exact: true })).toBeDisabled();
  await page.locator('#new-back').click();
  release();
  state.holdCreate = null;
  await page.locator('#new-next').click();
  await expect(page.getByRole('list', { name: 'Selected repositories' }).getByRole('listitem')).toHaveCount(2);
  await expect(page.locator('#new-progress')).toContainText('Project');
  expect(state.actions).toHaveLength(0);
  await page.getByRole('button', { name: 'Use project', exact: true }).click();
  await expect(page.locator('#new-progress')).toContainText('Review');
  expect(state.creates).toEqual([
    { sources: ['example/app', 'example/shared'] },
    { sources: ['example/app', 'example/shared'] },
  ]);
});
