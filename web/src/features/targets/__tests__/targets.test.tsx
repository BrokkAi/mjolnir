import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { ViewerTarget, ViewerTargetCapacity } from '../../../api/types';
import { useSessionStore } from '../../../stores/useSessionStore';
import { metadata, mockFetch } from '../../../test-utils';
import TargetsView from '../TargetsView';

const GiB = 1024 ** 3;

const target = (overrides: Partial<ViewerTarget> = {}): ViewerTarget => ({
  id: 't1',
  kind: 'container',
  permission_mode: 'default',
  yolo: false,
  subagents_configured: true,
  oracle_configured: false,
  resource_allocation_kind: 'container',
  requires_project_directory: true,
  remembered_container_size: { cpus: 4, memory_bytes: 8 * GiB },
  container_host_limits: { cpus: 16, memory_bytes: 64 * GiB },
  runtime_missing: false,
  default_candidate: true,
  availability: { status: 'available' },
  recent_project_directories: [],
  ...overrides,
});

const capacity = (overrides: Partial<ViewerTargetCapacity> = {}): ViewerTargetCapacity => ({
  id: 'host1',
  label: 'Build host',
  target_ids: ['t1'],
  cpu_percent: 42,
  memory_used_bytes: 12 * GiB,
  memory_total_bytes: 64 * GiB,
  logical_cores: 16,
  virtual_machines: 2,
  sampled_at_epoch_seconds: 1_700_000_000,
  refreshing: false,
  stale: false,
  has_error: false,
  container_capacity: {
    total: { cpus: 8, memory_bytes: 64 * GiB },
    reserved: { cpus: 1, memory_bytes: 4 * GiB },
    committed: { cpus: 3, memory_bytes: 24 * GiB },
    remaining: { cpus: 4, memory_bytes: 36 * GiB },
    complete: true,
  },
  storage: [
    {
      machine: 'build-1',
      mount: '/var/lib/mjolnir',
      available_bytes: 120 * GiB,
      reserved_bytes: 5 * GiB,
      condition: 'ok',
    },
    {
      machine: 'build-2',
      mount: '/srv/builds',
      available_bytes: 3 * GiB,
      reserved_bytes: 1 * GiB,
      condition: 'low',
      detail: 'Only 3 GiB remain on /srv/builds.',
    },
    {
      mount: '/tmp',
      available_bytes: 0,
      reserved_bytes: 0,
      condition: 'full',
      detail: 'The filesystem is full.',
    },
  ],
  ...overrides,
});

beforeEach(() => {
  useSessionStore.setState({ meta: metadata({ targets: [target()], capacity: [capacity()] }) });
});

afterEach(() => {
  cleanup();
  useSessionStore.setState({ meta: null });
  vi.unstubAllGlobals();
});

describe('TargetsView', () => {
  it('renders host capacity, storage meters and target verdicts', () => {
    useSessionStore.setState({
      meta: metadata({
        targets: [
          target(),
          target({
            id: 't2',
            kind: 'local',
            runtime_missing: true,
            default_candidate: false,
            resource_allocation_kind: 'fixed',
            availability: { status: 'missing', reason: 'Docker socket is unavailable.' },
          }),
          target({ id: 't3', availability: { status: 'unknown' }, default_candidate: false }),
        ],
        capacity: [capacity()],
        target_availability_notes: [
          {
            text: 'Fleet is at its configured limit',
            full: 'Two hosts are configured; raise fleet_size to add more.',
            routine: false,
          },
        ],
      }),
    });

    render(<TargetsView />);

    expect(screen.getByText('Build host')).toBeInTheDocument();
    expect(screen.getByLabelText('CPU')).toHaveAttribute('aria-valuenow', '42');
    expect(screen.getByText('12 GiB / 64 GiB')).toBeInTheDocument();
    expect(screen.getByText('16')).toBeInTheDocument();
    expect(screen.getByText('2')).toBeInTheDocument();
    expect(screen.getByText('/var/lib/mjolnir')).toBeInTheDocument();
    expect(screen.getByText('Only 3 GiB remain on /srv/builds.')).toBeInTheDocument();
    expect(screen.getByText('The filesystem is full.')).toBeInTheDocument();
    expect(screen.getAllByText('Low').length).toBeGreaterThan(0);
    expect(screen.getAllByText('Full').length).toBeGreaterThan(0);

    // Container capacity reports committed, reserved, remaining and totals.
    expect(screen.getByText('3 / 8 total')).toBeInTheDocument();
    expect(screen.getByText('24 GiB / 64 GiB')).toBeInTheDocument();
    expect(screen.getByText('36 GiB')).toBeInTheDocument();

    // Target verdicts and warnings.
    expect(screen.getByText('Fleet is at its configured limit')).toBeInTheDocument();
    expect(screen.getByText('Docker socket is unavailable.')).toBeInTheDocument();
    expect(screen.getByText('Runtime missing')).toBeInTheDocument();
    expect(screen.getByText('Default candidate')).toBeInTheDocument();
    expect(screen.getAllByText('16 CPUs · 64 GiB RAM').length).toBeGreaterThan(0);
    expect(screen.getAllByText('4 CPUs · 8 GiB RAM').length).toBeGreaterThan(0);
    expect(screen.getAllByText('Unknown').length).toBeGreaterThan(0);
  });

  it('dispatches check-availability and refresh-capacity actions', async () => {
    const fetchMock = mockFetch({
      '/api/v1/actions': () => new Response('', { status: 202 }),
    });
    render(<TargetsView />);

    fireEvent.click(screen.getByRole('button', { name: 'Check availability' }));
    await waitFor(() => expect(fetchMock.calls).toHaveLength(1));
    expect(JSON.parse(String(fetchMock.calls[0]?.init?.body))).toEqual({
      action: 'check-availability',
    });

    fireEvent.click(screen.getByRole('button', { name: 'Refresh capacity for t1' }));
    await waitFor(() => expect(fetchMock.calls).toHaveLength(2));
    expect(JSON.parse(String(fetchMock.calls[1]?.init?.body))).toEqual({
      action: 'refresh-capacity',
      target_id: 't1',
    });
  });
});
