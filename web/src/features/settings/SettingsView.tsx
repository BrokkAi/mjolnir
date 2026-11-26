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
  ApiError,
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

function normalized(value: JsonValue, top = false): JsonValue {
  if (Array.isArray(value)) return value.map(item => normalized(item));
  if (value !== null && typeof value === 'object') return Object.fromEntries(Object.keys(value)
    .filter(key => !top || !key.startsWith('_')).sort().map(key => [key, normalized(value[key])]));
  return value;
}

function object(value: JsonValue | undefined): Record<string, JsonValue> {
  return value !== null && typeof value === 'object' && !Array.isArray(value) ? value : {};
}

function optionsPending(options: JsonValue | undefined, entries: JsonValue | undefined): boolean {
  const values = object(options);
  return Array.isArray(entries) && entries.some((_, index) => {
    const option = values[String(index)];
    return option == null || object(option).efforts_pending === true;
  });
}

function discoveryPending(page: SettingsPage, draft: JsonValue, subagents: JsonValue, reviewers: JsonValue): boolean {
  const data = object(draft);
  if (page.path[0] === 'review') return pageActions(page).some(action => action.disabled_reason === 'Checking reviewer models…') || optionsPending(reviewers, object(data.review).reviewers);
  if (page.path[0] === 'subagents') {
    const tasks = object(data.subagents);
    const options = object(subagents);
    const names = page.path[1] && page.path[1] in tasks ? [page.path[1]] : Object.keys(tasks);
    return names.some(name => optionsPending(options[name], tasks[name]));
  }
  return false;
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

type EditOutcome = 'accepted' | 'refused' | 'failed';

function TextControl({
  row,
  disabled,
  onSet,
  onClearError,
  hidden = false,
}: {
  row: TextSettingRow;
  disabled: boolean;
  onSet: (text: string) => Promise<EditOutcome>;
  onClearError: () => void;
  hidden?: boolean;
}) {
  const [value, setValue] = useState(row.value);
  const valueRef = useRef(value);
  const inFlightCountRef = useRef(0);
  const queuedValueRef = useRef<string | null>(null);
  const dirtyLocalRef = useRef(false);

  useEffect(() => {
    if (inFlightCountRef.current === 0 && !dirtyLocalRef.current) {
      valueRef.current = row.value;
      setValue(row.value);
    }
  }, [row.value]);

  const apply = () => {
    const baseline = inFlightCountRef.current > 0 ? queuedValueRef.current : row.value;
    if (value === baseline) {
      dirtyLocalRef.current = false;
      if (inFlightCountRef.current === 0) onClearError();
      return;
    }
    if (hidden && value === '') return;
    dirtyLocalRef.current = false;
    inFlightCountRef.current += 1;
    queuedValueRef.current = value;
    void onSet(value).then(outcome => {
      inFlightCountRef.current = Math.max(0, inFlightCountRef.current - 1);
      if (inFlightCountRef.current === 0) queuedValueRef.current = null;
      if (hidden && outcome === 'accepted' && valueRef.current === value) {
        valueRef.current = '';
        setValue('');
      }
    });
  };

  return (
    <input
      type={hidden ? 'password' : row.number ? 'number' : 'text'}
      autoComplete={hidden ? 'new-password' : undefined}
      inputMode={row.number ? 'decimal' : undefined}
      aria-label={row.label}
      value={value}
      placeholder={row.placeholder}
      disabled={disabled}
      onChange={event => { dirtyLocalRef.current = true; valueRef.current = event.target.value; setValue(event.target.value); }}
      onBlur={apply}
      onKeyDown={event => {
        if (event.key === 'Enter') {
          event.preventDefault();
          apply();
        }
        if (event.key === 'Escape') { dirtyLocalRef.current = false; valueRef.current = row.value; setValue(row.value); onClearError(); }
      }}
      className="h-10 min-w-0 w-full rounded-lg border border-line bg-bg px-2.5 text-sm text-fg disabled:opacity-60 sm:w-56"
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
        className="flex min-w-0 max-w-full flex-wrap gap-1 rounded-lg bg-raised p-1"
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
              className={`min-h-8 min-w-0 max-w-full break-all rounded-md px-3 text-xs font-medium disabled:opacity-60 ${
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
      className="h-10 min-w-0 w-full rounded-lg border border-line bg-bg px-2 text-sm text-fg disabled:opacity-60 sm:w-56"
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
  onClearError,
  error,
}: {
  row: SettingsRow;
  disabled: boolean;
  onEdit: (edit: SettingsEdit) => Promise<EditOutcome>;
  onNavigate: (path: string[]) => void;
  onClearError: (path: string[]) => void;
  error?: string;
}) {

  const label = (
    <div className="min-w-0">
      <p className="break-words text-sm font-medium text-fg">{row.label}</p>
      {row.help && <p className="mt-0.5 text-xs text-muted">{row.help}</p>}
    </div>
  );

  if (row.control === 'page') {
    return (
      <button
        type="button"
        disabled={disabled}
        onClick={() => onNavigate(row.path)}
        className="flex w-full items-center gap-3 rounded-lg border border-line bg-surface p-3 text-left hover:bg-raised disabled:opacity-60"
      >
        <div className="min-w-0 flex-1">
          <p className="break-words text-sm font-medium text-fg">{row.label}</p>
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
            onClearError={() => onClearError(row.path)}
          />
        );
      case 'hidden':
        return <TextControl row={{ ...row, control: 'text', value: '', placeholder: 'Hidden · type to replace', number: false }} hidden disabled={disabled} onSet={text => onEdit({ kind: 'set', path: row.path, text })} onClearError={() => onClearError(row.path)} />;
      case 'read_only':
        return <span className="min-w-0 break-words text-sm text-muted">{row.text}</span>;
      default:
        return null;
    }
  })();

  return (
    <div className="min-w-0 rounded-lg border border-line bg-surface p-3">
      <div className="flex min-w-0 flex-col gap-2 sm:flex-row sm:items-center sm:justify-between">
        {label}
        <div className="flex min-w-0 w-full items-center sm:w-auto sm:justify-end">{control}</div>
      </div>
      {error && <p role="alert" className="mt-2 break-words text-xs text-danger">{error}</p>}
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
  const pathKeyRef = useRef(pathKey);
  pathKeyRef.current = pathKey;
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
  const [fieldErrors, setFieldErrors] = useState<Record<string, string>>({});
  const fieldErrorsRef = useRef<Record<string, string>>({});
  const [resetFields, setResetFields] = useState(0);
  const [subagentOptions, setSubagentOptions] = useState<JsonValue>(null);
  const [reviewerOptions, setReviewerOptions] = useState<JsonValue>(null);
  const [pollGeneration, setPollGeneration] = useState(0);

  const pagePathRef = useRef<string>('[]');
  const draftRef = useRef<JsonValue>(null);
  const originalRef = useRef<JsonValue>(null);
  // The persisted original omits defaults that the editable draft expands.
  const cleanDraftRef = useRef<JsonValue>(null);
  const queueRef = useRef<Promise<void>>(Promise.resolve());
  const epochRef = useRef(0);
  const pageRef = useRef<SettingsPage | null>(null);
  const pollWindowRef = useRef<{ key: string; generation: number; deadline: number } | null>(null);
  const pollSuppressedRef = useRef(false);
  const lastRouteRef = useRef(pathKey);

  const enqueue = useCallback((work: (valid: () => boolean) => Promise<void>) => {
    const epoch = epochRef.current;
    const valid = () => epoch === epochRef.current;
    const next = queueRef.current.then(async () => { if (valid()) await work(valid); });
    queueRef.current = next.catch(() => {});
    return next;
  }, []);

  const applyPage = useCallback((next: SettingsPage) => {
    pagePathRef.current = JSON.stringify(next.path);
    pageRef.current = next;
    setPage(next);
  }, []);

  const applyOptions = useCallback((draft: JsonValue) => {
    const data = object(draft);
    setSubagentOptions(data._subagent_options ?? null);
    setReviewerOptions(data._reviewer_options ?? null);
  }, []);

  useEffect(() => {
    ++epochRef.current;
    setLoading(true);
    setLoadError(null);
    void enqueue(async valid => {
      try {
        const result = await getSettings();
        if (!valid()) return;
        setDocument(result);
        draftRef.current = result.draft;
        originalRef.current = result.original;
        cleanDraftRef.current = result.draft;
        applyOptions(result.draft);
        applyPage(result.page);
        setLoading(false);
      } catch (failure) {
        if (valid()) { setLoadError(describe(failure)); setLoading(false); }
      }
    });
    return () => { ++epochRef.current; };
  }, [applyOptions, applyPage, enqueue, loadNonce]);

  useEffect(() => {
    if (loading || !document) return;
    if (lastRouteRef.current !== pathKey) {
      lastRouteRef.current = pathKey;
      pollSuppressedRef.current = false;
    }
    if (pagePathRef.current === pathKey) {
      setPageLoading(false);
      return;
    }
    let cancelled = false;
    const target = requestedPathRef.current;
    setPageLoading(true);
    setLoadError(null);
    setPollGeneration(value => value + 1);
    void enqueue(async valid => {
      if (cancelled) return;
      try {
        const result = await getSettingsPage(draftRef.current, target);
        if (!valid() || cancelled) return;
        applyPage(result.page);
        setSubagentOptions(result.subagent_options);
        setReviewerOptions(result.reviewer_options);
      } catch (failure) {
        if (!valid() || cancelled) return;
        if (failure instanceof ApiError && failure.status === 404 && target.length) {
          navigate({ name: 'settings', path: [] });
        } else setLoadError(describe(failure));
      } finally {
        if (valid() && !cancelled) setPageLoading(false);
      }
    });
    return () => { cancelled = true; };
  }, [applyPage, document, enqueue, loading, navigate, pathKey]);

  const dirty = document !== null && JSON.stringify(normalized(document.draft, true)) !== JSON.stringify(normalized(cleanDraftRef.current, true));

  useEffect(() => {
    if (!dirty) return;
    const warn = (event: BeforeUnloadEvent) => { event.preventDefault(); event.returnValue = ''; };
    window.addEventListener('beforeunload', warn);
    return () => window.removeEventListener('beforeunload', warn);
  }, [dirty]);

  useEffect(() => {
    if (pollSuppressedRef.current || !page || pagePathRef.current !== pathKey || !discoveryPending(page, draftRef.current, subagentOptions, reviewerOptions)) return;
    let cancelled = false;
    if (pollWindowRef.current?.key !== pathKey || pollWindowRef.current.generation !== pollGeneration) {
      pollWindowRef.current = { key: pathKey, generation: pollGeneration, deadline: Date.now() + 30_000 };
    }
    const deadline = pollWindowRef.current.deadline;
    let timer: ReturnType<typeof setTimeout>;
    const schedule = () => {
      if (!cancelled && Date.now() < deadline) timer = setTimeout(poll, 1000);
    };
    const poll = () => {
      void enqueue(async valid => {
        if (cancelled || Date.now() >= deadline || pagePathRef.current !== pathKey) return;
        try {
          const result = await getSettingsPage(draftRef.current, page.path);
          if (!valid() || cancelled || pagePathRef.current !== pathKey) return;
          applyPage(result.page);
          setSubagentOptions(result.subagent_options);
          setReviewerOptions(result.reviewer_options);
          if (discoveryPending(result.page, draftRef.current, result.subagent_options, result.reviewer_options)) schedule();
        } catch (failure) {
          if (valid() && !cancelled) {
            if (failure instanceof ApiError && failure.status === 404) navigate({ name: 'settings', path: [] });
            else setError(describe(failure));
          }
        }
      });
    };
    schedule();
    return () => { cancelled = true; clearTimeout(timer); };
  }, [page, pathKey, pollGeneration, enqueue, applyPage, navigate, subagentOptions, reviewerOptions]);

  const clearFieldError = (path: string[]) => {
    const key = JSON.stringify(path);
    if (!(key in fieldErrorsRef.current)) return;
    const next = { ...fieldErrorsRef.current };
    delete next[key];
    fieldErrorsRef.current = next;
    setFieldErrors(next);
  };

  const runEdit = async (edit: SettingsEdit): Promise<EditOutcome> => {
    const editPage = pageRef.current?.path;
    if (!editPage) return 'failed';
    const editPageKey = JSON.stringify(editPage);
    const structural = edit.kind === 'add' || edit.kind === 'remove' || edit.kind === 'action';
    if (structural) setBusy(true);
    setError(null);
    setNotice(null);
    pollSuppressedRef.current = false;
    setPollGeneration(value => value + 1);
    let outcome: EditOutcome = 'failed';
    await enqueue(async valid => {
      try {
        if (JSON.stringify(pageRef.current?.path ?? []) !== editPageKey || pathKeyRef.current !== editPageKey) return;
        const result = await editSettings(draftRef.current, editPage, edit);
        if (!valid()) return;
        draftRef.current = result.draft;
        setDocument(current => current ? { ...current, draft: result.draft } : current);
        applyOptions(result.draft);
        if (edit.kind === 'set' || edit.kind === 'toggle') {
          const key = JSON.stringify(edit.path);
          const next = { ...fieldErrorsRef.current };
          if (result.error) next[key] = result.error; else delete next[key];
          fieldErrorsRef.current = next;
          setFieldErrors(next);
        } else if (result.error) setError(result.error);
        if (!result.error && edit.kind === 'remove') {
          const prefix = /^\d+$/.test(edit.path.at(-1) ?? '') ? edit.path.slice(0, -1) : edit.path;
          const next = Object.fromEntries(Object.entries(fieldErrorsRef.current).filter(([key]) => {
            const path = JSON.parse(key) as string[];
            return !prefix.every((segment, index) => path[index] === segment);
          }));
          fieldErrorsRef.current = next;
          setFieldErrors(next);
        }
        if (!result.error && result.notice) setNotice(result.notice);
        if (pathKeyRef.current === editPageKey) {
          applyPage(result.page);
          if (JSON.stringify(result.page.path) !== editPageKey) navigate({ name: 'settings', path: result.page.path });
        }
        outcome = result.error ? 'refused' : 'accepted';
      } catch (failure) {
        if (valid()) {
          const message = describe(failure);
          setError(message);
          if (edit.kind === 'set' || edit.kind === 'toggle') {
            const next = { ...fieldErrorsRef.current, [JSON.stringify(edit.path)]: message };
            fieldErrorsRef.current = next;
            setFieldErrors(next);
          }
        }
      } finally {
        if (valid() && structural) setBusy(false);
      }
    });
    return outcome;
  };

  const discard = () => {
    ++epochRef.current;
    setBusy(true);
    setError(null);
    setNotice(null);
    fieldErrorsRef.current = {};
    setFieldErrors({});
    pollSuppressedRef.current = true;
    setPollGeneration(value => value + 1);
    void enqueue(async valid => {
      try {
        const currentPath = pageRef.current?.path ?? [];
        let result;
        try {
          result = await getSettingsPage(cleanDraftRef.current, currentPath);
        } catch (pageFailure) {
          if (pageFailure instanceof ApiError && pageFailure.status === 404 && currentPath.length) {
            result = await getSettingsPage(cleanDraftRef.current, []);
          } else {
            throw pageFailure;
          }
        }
        if (!valid()) return;
        draftRef.current = cleanDraftRef.current;
        setDocument(current => current ? { ...current, draft: cleanDraftRef.current } : current);
        applyPage(result.page);
        setSubagentOptions(result.subagent_options);
        setReviewerOptions(result.reviewer_options);
        setResetFields(value => value + 1);
        setNotice('Changes discarded.');
        if (JSON.stringify(result.page.path) !== JSON.stringify(currentPath)) {
          navigate({ name: 'settings', path: result.page.path });
        }
      } catch (failure) { if (valid()) setError(describe(failure)); }
      finally { if (valid()) setBusy(false); }
    });
  };

  const save = () => {
    setBusy(true);
    setError(null);
    setNotice(null);
    void enqueue(async valid => {
      try {
        if (Object.keys(fieldErrorsRef.current).length) {
          setError('Fix the rejected settings before saving.');
          return;
        }
        const currentPath = pageRef.current?.path ?? [];
        const saved = await saveSettings(originalRef.current, draftRef.current, currentPath);
        if (!valid()) return;
        draftRef.current = saved.draft;
        originalRef.current = saved.original;
        cleanDraftRef.current = saved.draft;
        setDocument(saved);
        applyOptions(saved.draft);
        applyPage(saved.page);
        fieldErrorsRef.current = {};
        setFieldErrors({});
        setNotice('Settings saved.');
        pushToast('success', 'Settings saved.');
        if (JSON.stringify(saved.page.path) !== JSON.stringify(currentPath)) {
          navigate({ name: 'settings', path: saved.page.path });
        }
      } catch (failure) { if (valid()) setError(describe(failure)); }
      finally { if (valid()) setBusy(false); }
    });
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
            <div key={action.id} className="min-w-0">
            <button
              type="button"
              disabled={busy || pageLoading || Boolean(action.disabled_reason)}
              title={action.disabled_reason}
              onClick={() => void runEdit({ kind: 'action', id: action.id })}
              className="h-10 rounded-lg border border-line px-3 text-sm font-medium text-fg hover:bg-raised disabled:opacity-50"
            >
              {action.label}
            </button>
            {action.disabled_reason && <p className="mt-1 max-w-xs break-words text-xs text-muted">{action.disabled_reason}</p>}
            </div>
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
                key={`${JSON.stringify(row.path)}-${resetFields}`}
                row={row}
                error={fieldErrors[JSON.stringify(row.path)]}
                disabled={busy || pageLoading}
                onEdit={runEdit}
                onNavigate={navigateTo}
                onClearError={clearFieldError}
              />
            ))}
          </div>
        </section>
      ))}

      {add && (
        <section aria-label="Add entry" className="flex flex-col gap-2">
          <AddEntryControl
            add={add}
            disabled={busy || pageLoading}
            onAdd={name => void runEdit({ kind: 'add', path: page.path, name })}
          />
        </section>
      )}

      {page.removable && (
        <button
          type="button"
          disabled={busy || pageLoading}
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
