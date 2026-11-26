import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { CHUNK_RELOAD_KEY, CHUNK_RELOAD_WINDOW_MS } from '../chunkReload';
import { RouteErrorBoundary } from '../components/shell/RouteOutlet';

const reload = vi.fn();

function Broken({ message }: { message: string }): never {
  throw new Error(message);
}

beforeEach(() => {
  sessionStorage.clear();
  reload.mockClear();
  vi.stubGlobal('location', { ...window.location, reload });
  vi.spyOn(console, 'error').mockImplementation(() => {});
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe('RouteErrorBoundary', () => {
  it('reloads once for a stale lazy chunk, then shows a manual reload card instead of looping', () => {
    const stale = 'Failed to fetch dynamically imported module: /assets/DashboardView-abc.js';
    const first = render(
      <RouteErrorBoundary resetKey="a">
        <Broken message={stale} />
      </RouteErrorBoundary>,
    );
    expect(reload).toHaveBeenCalledTimes(1);
    expect(sessionStorage.getItem(CHUNK_RELOAD_KEY)).not.toBeNull();
    expect(screen.queryByRole('button', { name: 'Reload Mjolnir' })).not.toBeInTheDocument();
    first.unmount();

    // The reloaded page fails the same way inside the window: no second automatic reload.
    render(
      <RouteErrorBoundary resetKey="a">
        <Broken message={stale} />
      </RouteErrorBoundary>,
    );
    expect(reload).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByRole('button', { name: 'Reload Mjolnir' }));
    expect(reload).toHaveBeenCalledTimes(2);
  });

  it('allows another automatic reload once the window has passed', () => {
    sessionStorage.setItem(CHUNK_RELOAD_KEY, String(Date.now() - CHUNK_RELOAD_WINDOW_MS - 1));
    render(
      <RouteErrorBoundary resetKey="a">
        <Broken message="Importing a module script failed." />
      </RouteErrorBoundary>,
    );
    expect(reload).toHaveBeenCalledTimes(1);
  });

  it('shows the reload card without reloading for any other render error', () => {
    render(
      <RouteErrorBoundary resetKey="a">
        <Broken message="x is not a function" />
      </RouteErrorBoundary>,
    );
    expect(reload).not.toHaveBeenCalled();
    expect(screen.getByRole('alert')).toHaveTextContent('This screen could not load.');
    expect(screen.getByRole('button', { name: 'Reload Mjolnir' })).toBeInTheDocument();
  });
});
