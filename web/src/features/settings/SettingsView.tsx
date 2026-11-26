import {
  AlertCircle,
  CheckCircle2,
  ChevronRight,
  LoaderCircle,
  RotateCcw,
  Save,
} from 'lucide-react';
import { useCallback, useEffect, useRef, useState } from 'react';
import {
  editSettings,
  getSettings,
  getSettingsPage,
  saveSettings,
} from '../../api/client';
import type {
  JsonValue,
  SettingsDocument,
  SettingsEdit,
  SettingsPage,
  SettingsRow,
} from '../../api/types';
import { useSessionStore } from '../../stores/useSessionStore';
import { useUiStore, type Theme } from '../../stores/useUiStore';
import { GitIdentityCard } from './GitIdentityCard';

/** Props the shell passes. Keep this shape when replacing the placeholder. */
export type SettingsViewProps = { path: string[] };

interface SettingsActionView {
  id: string;
  label: string;
  disabled_reason?: string;
}

interface SettingsAddView {
  label: string;
  named: boolean;
}

type ChoiceSettingRow = SettingsRow & {
  control: 'choice';
  value: JsonValue;
  choices: { value: JsonValue; label: string }[];
};
type TextSettingRow = SettingsRow & {
  control: 'text';
  value: string;
  placeholder: string;
  number: boolean;
};

const THEMES: { value: Theme; label: string }[] = [
  { value: 'system', label: 'System' },
  { value: 'dark', label: 'Dark' },
  { value: 'light', label: 'Light' },
];

function describe(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function choiceKey(value: JsonValue): string {
  return JSON.stringify(value);
}

function pageActions(page: SettingsPage): SettingsActionView[] {
  if (!Array.isArray(page.actions)) return [];
  return (page.actions as unknown as SettingsActionView[]).filter(
    action => action && typeof action.id === 'string' && typeof action.label === 'string',
  );
}

function pageAdd(page: SettingsPage): SettingsAddView | null {
  const add = page.add;
  if (add === undefined || add === null || typeof add !== 'object' || Array.isArray(add)) {
    return null;
  }
  const record = add as { label?: unknown; named?: unknown };
  if (typeof record.label !== 'string') return null;
  return { label: record.label, named: record.named === true };
}

function TextControl({
  row,
  disabled,
  onSet,
}: {
  row: TextSettingRow;
  disabled: boolean;
  onSet: (text: string) => void;
}) {
  const [value, setValue] = useState(row.value);
  // A server answer may replace the value (for example after an edit).
  useEffect(() => setValue(row.value), [row.value]);

  const apply = () => {
    if (value !== row.value) onSet(value);
  };

  return (
    <input
      type={row.number ? 'number' : 'text'}
      inputMode={row.number ? 'decimal' : undefined}
      aria-label={row.label}
      value={value}
      placeholder={row.placeholder}
      disabled={disabled}
      onChange={event => setValue(event.target.value)}
      onBlur={apply}
      onKeyDown={event => {
        if (event.key === 'Enter') {
          event.preventDefault();
          apply();
        }
        if (event.key === 'Escape') setValue(row.value);
      }}
      className="h-10 w-full rounded-lg border border-line bg-bg px-2.5 text-sm text-fg disabled:opacity-60 sm:w-56"
    />
  );
}

function ChoiceControl({
  row,
  disabled,
  onSet,
}: {
  row: ChoiceSettingRow;
  disabled: boolean;
  onSet: (choice: JsonValue) => void;
}) {
  const current = choiceKey(row.value);
  if (row.choices.length <= 3) {
    return (
      <div
        role="radiogroup"
        aria-label={row.label}
        className="flex flex-wrap gap-1 rounded-lg bg-raised p-1"
      >
        {row.choices.map(choice => {
          const selected = choiceKey(choice.value) === current;
          return (
            <button
              key={choiceKey(choice.value)}
              type="button"
              role="radio"
              aria-checked={selected}
              disabled={disabled}
              onClick={() => {
                if (!selected) onSet(choice.value);
              }}
              className={`h-8 rounded-md px-3 text-xs font-medium disabled:opacity-60 ${
                selected ? 'bg-surface text-fg shadow-sm' : 'text-muted hover:text-fg'
              }`}
            >
              {choice.label}
            </button>
          );
        })}
      </div>
    );
  }
  return (
    <select
      aria-label={row.label}
      value={current}
      disabled={disabled}
      onChange={event => {
        const chosen = row.choices.find(choice => choiceKey(choice.value) === event.target.value);
        if (chosen) onSet(chosen.value);
      }}
      className="h-10 w-full rounded-lg border border-line bg-bg px-2 text-sm text-fg disabled:opacity-60 sm:w-56"
    >
      {row.choices.map(choice => (
        <option key={choiceKey(choice.value)} value={choiceKey(choice.value)}>
          {choice.label}
        </option>
      ))}
    </select>
  );
}

function AddEntryControl({
  add,
  disabled,
  onAdd,
}: {
  add: SettingsAddView;
  disabled: boolean;
  onAdd: (name: string | undefined) => void;
}) {
  const [name, setName] = useState('');
  const submit = () => onAdd(add.named ? name.trim() : undefined);
  return (
    <div className="flex flex-wrap items-center gap-2">
      {add.named && (
        <input
          type="text"
          aria-label={`Name for new ${add.label.toLowerCase()}`}
          placeholder="Name"
          value={name}
          disabled={disabled}
          onChange={event => setName(event.target.value)}
          onKeyDown={event => {
            if (event.key === 'Enter') {
              event.preventDefault();
              submit();
            }
          }}
          className="h-10 min-w-0 flex-1 rounded-lg border border-line bg-bg px-2.5 text-sm text-fg sm:max-w-64"
        />
      )}
      <button
        type="button"
        disabled={disabled}
        onClick={submit}
        className="h-10 rounded-lg border border-line px-3 text-sm font-medium text-fg hover:bg-raised disabled:opacity-50"
      >
        {add.label}
      </button>
    </div>
  );
}

function SettingRowView({
  row,
  disabled,
  onEdit,
  onNavigate,
}: {
  row: SettingsRow;
  disabled: boolean;
  onEdit: (edit: SettingsEdit) => void;
  onNavigate: (path: string[]) => void;
}) {
  if (row.control === 'hidden') return null;

  const label = (
    <div className="min-w-0">
      <p className="truncate text-sm font-medium text-fg">{row.label}</p>
      {row.help && <p className="mt-0.5 text-xs text-muted">{row.help}</p>}
    </div>
  );

  if (row.control === 'page') {
    return (
      <button
        type="button"
        onClick={() => onNavigate(row.path)}
        className="flex w-full items-center gap-3 rounded-lg border border-line bg-surface p-3 text-left hover:bg-raised"
      >
        <div className="min-w-0 flex-1">
          <p className="truncate text-sm font-medium text-fg">{row.label}</p>
          <p className="mt-0.5 truncate text-xs text-muted">{row.summary}</p>
          {row.help && <p className="mt-0.5 text-xs text-muted">{row.help}</p>}
        </div>
        <ChevronRight className="size-4 shrink-0 text-muted" aria-hidden />
      </button>
    );
  }

  const control = (() => {
    switch (row.control) {
      case 'checkbox':
        return (
          <button
            type="button"
            role="switch"
            aria-checked={row.checked}
            aria-label={row.label}
            disabled={disabled}
            onClick={() => onEdit({ kind: 'toggle', path: row.path })}
            className={`relative h-6 w-11 shrink-0 rounded-full transition-colors disabled:opacity-60 ${
              row.checked ? 'bg-accent' : 'bg-raised'
            }`}
          >
            <span
              aria-hidden
              className={`absolute top-0.5 size-5 rounded-full bg-surface shadow transition-[left] ${
                row.checked ? 'left-[22px]' : 'left-0.5'
              }`}
            />
          </button>
        );
      case 'choice':
        return (
          <ChoiceControl
            row={row}
            disabled={disabled}
            onSet={choice => onEdit({ kind: 'set', path: row.path, choice })}
          />
        );
      case 'text':
        return (
          <TextControl
            row={row}
            disabled={disabled}
            onSet={text => onEdit({ kind: 'set', path: row.path, text })}
          />
        );
      case 'read_only':
        return <span className="text-sm text-muted">{row.text}</span>;
      default:
        return null;
    }
  })();

  return (
    <div className="flex flex-wrap items-center justify-between gap-2 rounded-lg border border-line bg-surface p-3">
      {label}
      <div className="flex min-w-0 items-center justify-end">{control}</div>
    </div>
  );
}

function ThemePreference() {
  const theme = useUiStore(state => state.theme);
  const setTheme = useUiStore(state => state.setTheme);
  return (
    <div className="flex flex-col gap-2">
      <p className="text-sm font-medium">Theme</p>
      <div role="radiogroup" aria-label="Theme preference" className="grid grid-cols-3 gap-1 rounded-lg bg-raised p-1">
        {THEMES.map(option => (
          <button
            key={option.value}
            type="button"
            role="radio"
            aria-checked={theme === option.value}
            onClick={() => setTheme(option.value)}
            className={`h-9 rounded-md text-xs font-medium ${
              theme === option.value ? 'bg-surface text-fg shadow-sm' : 'text-muted hover:text-fg'
            }`}
          >
            {option.label}
          </button>
        ))}
      </div>
    </div>
  );
}

export default function SettingsView({ path }: SettingsViewProps) {
  const navigate = useUiStore(state => state.navigate);
  const pushToast = useUiStore(state => state.pushToast);
  const signOut = useSessionStore(state => state.signOut);

  const requestedPath = JSON.parse(JSON.stringify(path)) as string[];
  const pathKey = JSON.stringify(requestedPath);
  const requestedPathRef = useRef(requestedPath);
  requestedPathRef.current = requestedPath;

  const [document, setDocument] = useState<SettingsDocument | null>(null);
  const [page, setPage] = useState<SettingsPage | null>(null);
  const [loading, setLoading] = useState(true);
  const [pageLoading, setPageLoading] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [loadNonce, setLoadNonce] = useState(0);

  const pagePathRef = useRef<string>('[]');
  const syncedPathRef = useRef<string | null>(null);
  const draftRef = useRef<JsonValue>(null);

  const applyPage = useCallback((next: SettingsPage) => {
    pagePathRef.current = JSON.stringify(next.path);
    setPage(next);
  }, []);

  useEffect(() => {
    const controller = new AbortController();
    let cancelled = false;
    setLoading(true);
    setLoadError(null);
    getSettings(controller.signal)
      .then(result => {
        if (cancelled) return;
        setDocument(result);
        draftRef.current = result.draft;
        applyPage(result.page);
        setLoading(false);
      })
      .catch(failure => {
        if (cancelled || controller.signal.aborted) return;
        setLoadError(describe(failure));
        setLoading(false);
      });
    return () => {
      cancelled = true;
      controller.abort();
    };
  }, [applyPage, loadNonce]);

  // The route owns the page path. Load that page from the server whenever the
  // hash names one this draft has not rendered yet.
  useEffect(() => {
    if (loading || !document) return;
    if (syncedPathRef.current === pathKey) return;
    if (pagePathRef.current === pathKey) {
      syncedPathRef.current = pathKey;
      return;
    }
    const controller = new AbortController();
    let cancelled = false;
    setPageLoading(true);
    setLoadError(null);
    getSettingsPage(draftRef.current, requestedPathRef.current, controller.signal)
      .then(result => {
        if (cancelled) return;
        syncedPathRef.current = pathKey;
        applyPage(result.page);
        setPageLoading(false);
      })
      .catch(failure => {
        if (cancelled || controller.signal.aborted) return;
        setLoadError(describe(failure));
        setPageLoading(false);
      });
    return () => {
      cancelled = true;
      controller.abort();
    };
  }, [applyPage, document, loading, pathKey]);

  const dirty =
    document !== null && JSON.stringify(document.draft) !== JSON.stringify(document.original);

  const runEdit = async (edit: SettingsEdit) => {
    if (!document || !page || busy) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const result = await editSettings(document.draft, page.path, edit);
      draftRef.current = result.draft;
      setDocument(current => (current ? { ...current, draft: result.draft } : current));
      applyPage(result.page);
      if (result.error) setError(result.error);
      else if (result.notice) setNotice(result.notice);
      if (JSON.stringify(result.page.path) !== JSON.stringify(page.path)) {
        navigate({ name: 'settings', path: result.page.path });
      }
    } catch (failure) {
      setError(describe(failure));
    } finally {
      setBusy(false);
    }
  };

  const discard = async () => {
    if (!document || !page || busy) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const result = await getSettingsPage(document.original, page.path);
      draftRef.current = document.original;
      setDocument({ ...document, draft: document.original });
      applyPage(result.page);
      setNotice('Changes discarded.');
    } catch (failure) {
      setError(describe(failure));
    } finally {
      setBusy(false);
    }
  };

  const save = async () => {
    if (!document || !page || busy) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const saved = await saveSettings(document.original, document.draft, page.path);
      draftRef.current = saved.draft;
      setDocument(saved);
      applyPage(saved.page);
      setNotice('Settings saved.');
      pushToast('success', 'Settings saved.');
    } catch (failure) {
      setError(describe(failure));
    } finally {
      setBusy(false);
    }
  };

  const navigateTo = (next: string[]) => navigate({ name: 'settings', path: next });

  if (loading) {
    return (
      <section className="mx-auto flex max-w-2xl flex-col gap-3 p-4" aria-busy="true">
        <h2 className="text-lg font-semibold tracking-tight">Settings</h2>
        <p role="status" className="flex items-center gap-2 text-sm text-muted">
          <LoaderCircle className="size-4 animate-spin" aria-hidden /> Loading settings…
        </p>
      </section>
    );
  }

  if (loadError && (!document || !page)) {
    return (
      <section className="mx-auto flex max-w-2xl flex-col gap-3 p-4">
        <h2 className="text-lg font-semibold tracking-tight">Settings</h2>
        <p role="alert" className="rounded-lg bg-danger/10 p-2.5 text-sm text-danger">
          Could not load settings: {loadError}
        </p>
        <button
          type="button"
          onClick={() => setLoadNonce(value => value + 1)}
          className="h-10 self-start rounded-lg bg-accent px-3 text-sm font-semibold text-accent-fg"
        >
          Try again
        </button>
      </section>
    );
  }

  if (!document || !page) return null;

  const actions = pageActions(page);
  const add = pageAdd(page);
  const breadcrumb = page.breadcrumb.length ? page.breadcrumb : ['Settings'];
  const lastCrumb = breadcrumb.length - 1;

  return (
    <section className="mx-auto flex w-full max-w-3xl flex-col gap-3 p-3 pb-6 sm:p-4">
      <nav aria-label="Breadcrumb">
        <ol className="flex flex-wrap items-center gap-1 text-xs text-muted">
          {breadcrumb.map((crumb, index) => {
            const target = page.path.slice(0, index);
            const current = index === lastCrumb;
            return (
              <li key={`${crumb}-${index}`} className="flex items-center gap-1">
                {index > 0 && (
                  <span aria-hidden className="text-muted">
                    /
                  </span>
                )}
                {current ? (
                  <span aria-current="page" className="font-medium text-fg">
                    {crumb}
                  </span>
                ) : (
                  <button
                    type="button"
                    onClick={() => navigateTo(target)}
                    className="rounded px-1 py-0.5 hover:bg-raised hover:text-fg"
                  >
                    {crumb}
                  </button>
                )}
              </li>
            );
          })}
        </ol>
      </nav>

      {page.help && <p className="text-xs text-muted">{page.help}</p>}

      {page.notices?.length ? (
        <ul className="flex flex-col gap-1.5" aria-label="Notices">
          {page.notices.map((text, index) => (
            <li
              key={`${text}-${index}`}
              className="rounded-lg bg-warn/10 p-2 text-xs text-warn"
              role="status"
            >
              {text}
            </li>
          ))}
        </ul>
      ) : null}

      {(error || loadError) && (
        <p
          role="alert"
          className="flex items-start gap-1.5 rounded-lg bg-danger/10 p-2.5 text-sm text-danger"
        >
          <AlertCircle className="mt-0.5 size-4 shrink-0" aria-hidden />
          {error ?? loadError}
        </p>
      )}
      {notice && (
        <p role="status" className="flex items-center gap-1.5 rounded-lg bg-ok/10 p-2.5 text-sm text-ok">
          <CheckCircle2 className="size-4" aria-hidden />
          {notice}
        </p>
      )}

      {pageLoading && (
        <p role="status" className="flex items-center gap-2 text-xs text-muted">
          <LoaderCircle className="size-3.5 animate-spin" aria-hidden /> Loading page…
        </p>
      )}

      {actions.length > 0 && (
        <div className="flex flex-wrap gap-2">
          {actions.map(action => (
            <button
              key={action.id}
              type="button"
              disabled={busy || Boolean(action.disabled_reason)}
              title={action.disabled_reason}
              onClick={() => void runEdit({ kind: 'action', id: action.id })}
              className="h-10 rounded-lg border border-line px-3 text-sm font-medium text-fg hover:bg-raised disabled:opacity-50"
            >
              {action.label}
            </button>
          ))}
        </div>
      )}

      {page.groups.map((group, groupIndex) => (
        <section
          key={group.heading ?? `group-${groupIndex}`}
          aria-label={group.heading}
          className="flex flex-col gap-2"
        >
          {group.heading && (
            <h3 className="text-xs font-semibold uppercase tracking-wide text-muted">
              {group.heading}
            </h3>
          )}
          <div className="flex flex-col gap-2">
            {group.rows.map(row => (
              <SettingRowView
                key={JSON.stringify(row.path)}
                row={row}
                disabled={busy}
                onEdit={edit => void runEdit(edit)}
                onNavigate={navigateTo}
              />
            ))}
          </div>
        </section>
      ))}

      {add && (
        <section aria-label="Add entry" className="flex flex-col gap-2">
          <AddEntryControl
            add={add}
            disabled={busy}
            onAdd={name => void runEdit({ kind: 'add', path: page.path, name })}
          />
        </section>
      )}

      {page.removable && (
        <button
          type="button"
          disabled={busy}
          onClick={() => void runEdit({ kind: 'remove', path: page.path })}
          className="h-10 self-start rounded-lg border border-danger/40 px-3 text-sm font-medium text-danger hover:bg-danger/10 disabled:opacity-50"
        >
          Remove this entry
        </button>
      )}

      {page.path.length === 0 && (
        <>
          <section
            aria-labelledby="appearance-account-heading"
            className="flex flex-col gap-3 rounded-xl border border-line bg-surface p-3.5"
          >
            <h3 id="appearance-account-heading" className="text-sm font-semibold">
              Appearance and account
            </h3>
            <ThemePreference />
            <button
              type="button"
              onClick={() => void signOut()}
              className="h-10 self-start rounded-lg border border-line px-3 text-sm font-medium text-muted hover:bg-raised hover:text-fg"
            >
              Sign out
            </button>
          </section>
          <GitIdentityCard />
        </>
      )}

      {dirty && (
        <footer className="sticky bottom-0 z-10 -mx-3 flex flex-wrap items-center justify-between gap-2 border-t border-line bg-surface/95 p-3 backdrop-blur sm:-mx-4 sm:px-4">
          <p className="text-sm font-medium text-warn">Unsaved changes</p>
          <div className="flex gap-2">
            <button
              type="button"
              disabled={busy}
              onClick={() => void discard()}
              className="inline-flex h-10 items-center gap-1.5 rounded-lg border border-line px-3 text-sm font-medium text-fg hover:bg-raised disabled:opacity-50"
            >
              <RotateCcw className="size-4" aria-hidden />
              Discard
            </button>
            <button
              type="button"
              disabled={busy}
              onClick={() => void save()}
              className="inline-flex h-10 items-center gap-1.5 rounded-lg bg-accent px-3 text-sm font-semibold text-accent-fg disabled:opacity-50"
            >
              {busy ? (
                <LoaderCircle className="size-4 animate-spin" aria-hidden />
              ) : (
                <Save className="size-4" aria-hidden />
              )}
              Save
            </button>
          </div>
        </footer>
      )}
    </section>
  );
}
