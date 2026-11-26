import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { ViewerProfile } from '../../../api/types';
import { useSessionStore } from '../../../stores/useSessionStore';
import { metadata, mockFetch } from '../../../test-utils';
import QuotaView from '../QuotaView';

const NOW_SECONDS = 1_800_000_000;

const profile = (overrides: Partial<ViewerProfile> = {}): ViewerProfile => ({
  id: 'p1',
  availability: { status: 'available' },
  capabilities_key: 'p1:claude',
  harness_kind: 'claude',
  quota_pools: ['claude', 'anthropic'],
  quota: {
    summary: 'Claude usage',
    windows: [
      {
        label: '5-hour window',
        pool: 'claude',
        resets_at_epoch_seconds: NOW_SECONDS + 5_400,
        reset_countdown_style: 'five_hour',
        banked_resets: 2,
        percent_used: 82,
        projects_exhaustion_before_reset: true,
      },
      {
        label: 'Weekly window',
        pool: 'claude',
        resets_at_epoch_seconds: NOW_SECONDS + 2 * 86_400 + 4 * 3_600,
        reset_countdown_style: 'long',
        banked_resets: 0,
        fable_remaining_percent: 35,
        projects_exhaustion_before_reset: false,
      },
    ],
    resets_at: '2027-01-01T00:00:00Z',
    stale: false,
    refreshed_at_epoch_seconds: NOW_SECONDS - 60,
    next_check_at_epoch_seconds: NOW_SECONDS + 120,
    has_error: false,
  },
  ...overrides,
});

beforeEach(() => {
  useSessionStore.setState({
    meta: metadata({
      server_time_ms: NOW_SECONDS * 1000,
      profiles: [
        profile(),
        profile({
          id: 'p2',
          harness_kind: 'codex',
          capabilities_key: 'p2:codex',
          availability: { status: 'missing', reason: 'The API key is not configured.' },
          quota_pools: undefined,
          quota: {
            summary: 'No quota reported',
            stale: true,
            refreshed_at_epoch_seconds: NOW_SECONDS - 600,
            has_error: true,
          },
        }),
      ],
    }),
  });
});

afterEach(() => {
  cleanup();
  useSessionStore.setState({ meta: null });
  vi.unstubAllGlobals();
});

describe('QuotaView', () => {
  it('renders quota windows, countdowns, exhaustion warnings and error states', () => {
    render(<QuotaView />);

    expect(screen.getByText('p1')).toBeInTheDocument();
    expect(screen.getAllByText('claude').length).toBeGreaterThan(0);
    expect(screen.getAllByText('Available').length).toBeGreaterThan(0);
    expect(screen.getByText('Claude usage')).toBeInTheDocument();

    const fiveHour = screen.getByLabelText('5-hour window usage');
    expect(fiveHour).toHaveAttribute('aria-valuenow', '82');
    expect(screen.getByText('82% used')).toBeInTheDocument();
    expect(screen.getByText('Projects may exhaust before reset')).toBeInTheDocument();
    expect(screen.getByText('2 banked resets')).toBeInTheDocument();
    expect(screen.getByText('Resets in 1h 30m')).toBeInTheDocument();

    // A `fable_remaining_percent` window shows remaining, with the bar filled by usage.
    const weekly = screen.getByLabelText('Weekly window usage');
    expect(weekly).toHaveAttribute('aria-valuenow', '65');
    expect(screen.getByText('35% remaining')).toBeInTheDocument();
    expect(screen.getByText('65% used')).toBeInTheDocument();
    expect(screen.getByText('Resets in 2d 4h')).toBeInTheDocument();

    expect(screen.getByText('The API key is not configured.')).toBeInTheDocument();
    expect(screen.getByText('Missing')).toBeInTheDocument();
    expect(screen.getByText('Stale')).toBeInTheDocument();
    expect(screen.getByText('Last check failed')).toBeInTheDocument();
  });

  it('refreshes one profile quota at a time', async () => {
    const fetchMock = mockFetch({
      '/api/v1/actions': () => new Response('', { status: 202 }),
    });
    render(<QuotaView />);

    fireEvent.click(screen.getByRole('button', { name: 'Refresh quota for p2' }));
    await waitFor(() => expect(fetchMock.calls).toHaveLength(1));
    expect(JSON.parse(String(fetchMock.calls[0]?.init?.body))).toEqual({
      action: 'refresh-quota',
      profile_id: 'p2',
    });
  });
});
