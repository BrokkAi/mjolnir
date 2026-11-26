// One question control. Every option is a full-width touch row with a compact
// 20px indicator, so the text uses the screen instead of a wide empty gutter.
//
// Option descriptions and previews are first-class: descriptions render as
// sanitized Markdown, and a preview that runs past ~6 lines collapses behind
// an "Expand preview" toggle rather than pushing the answer off the sheet.

import { Check, ChevronDown, ChevronUp, Eye, EyeOff } from 'lucide-react';
import { useId, useState } from 'react';
import type { ElicitationField, ElicitationOption } from '../../api/types';
import { MarkdownContent } from '../../components/MarkdownContent';
import {
  FREE_TEXT_CHOICE,
  listValue,
  splitRecommended,
  textValue,
  type DraftValue,
} from './elicitationLogic';

export interface ElicitationFieldProps {
  field: ElicitationField;
  value: DraftValue | undefined;
  onChange: (value: DraftValue | undefined) => void;
  error?: string;
  disabled?: boolean;
  /** For a single-select whose companion is an Alternative. */
  customChoiceTitle?: string;
  onShortcut?: () => void;
  autoFocus?: boolean;
}

function FieldHeading({
  field,
  htmlFor,
}: {
  field: ElicitationField;
  htmlFor?: string;
}) {
  return (
    <div className="flex min-w-0 flex-wrap items-baseline gap-x-2 gap-y-1">
      <label htmlFor={htmlFor} className="min-w-0 break-words text-[15px] font-semibold text-fg">
        {field.title}
        {field.required && (
          <span className="ml-1 text-danger" aria-hidden>
            *
          </span>
        )}
        {field.required && <span className="sr-only"> (required)</span>}
      </label>
    </div>
  );
}

function FieldDescription({ text }: { text: string }) {
  return (
    <MarkdownContent
      text={text}
      compact
      className="mt-1 text-[13px] text-muted [&_p]:text-muted [&_li]:text-muted"
    />
  );
}

function FieldError({ id, message }: { id: string; message?: string }) {
  if (!message) return null;
  return (
    <p id={id} role="alert" className="mt-1.5 break-words text-[13px] font-medium text-danger">
      {message}
    </p>
  );
}

function OptionIndicator({ selected, multiple }: { selected: boolean; multiple: boolean }) {
  return (
    <span
      aria-hidden
      className={`mt-0.5 grid size-5 shrink-0 place-items-center border-2 transition-colors ${
        multiple ? 'rounded-[5px]' : 'rounded-full'
      } ${selected ? 'border-accent bg-accent text-accent-fg' : 'border-muted bg-transparent text-transparent'}`}
    >
      {selected && <Check className={multiple ? 'size-3.5' : 'size-3.5'} strokeWidth={3} />}
    </span>
  );
}

function OptionPreview({ text }: { text: string }) {
  const [expanded, setExpanded] = useState(false);
  const lineCount = text.split('\n').length;
  const longPreview = lineCount > 6 || text.length > 480;
  return (
    <div className="mt-2 min-w-0">
      <div className={longPreview && !expanded ? 'max-h-[7.5rem] overflow-hidden' : ''}>
        <pre className="overflow-x-auto overflow-y-hidden rounded-md border border-line bg-bg p-2 font-mono text-[12px] leading-relaxed text-fg [overflow-anchor:none]">
          <code>{text}</code>
        </pre>
      </div>
      {longPreview && (
        <button
          type="button"
          aria-expanded={expanded}
          onClick={event => {
            // The whole row is a label: keep the toggle from picking the option.
            event.preventDefault();
            event.stopPropagation();
            setExpanded(current => !current);
          }}
          className="mt-1 inline-flex min-h-11 items-center gap-1 rounded px-1 text-xs font-medium text-accent hover:underline"
        >
          {expanded ? <ChevronUp className="size-3.5" aria-hidden /> : <ChevronDown className="size-3.5" aria-hidden />}
          {expanded ? 'Collapse preview' : 'Expand preview'}
        </button>
      )}
    </div>
  );
}

function OptionBody({ option }: { option: ElicitationOption }) {
  const { title, recommended } = splitRecommended(option.title);
  return (
    <span className="min-w-0 flex-1">
      <span className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1">
        <span className="min-w-0 break-words text-[15px] font-medium text-fg">
          {title || option.value}
        </span>
        {recommended && (
          <span className="shrink-0 rounded-full border border-accent/40 bg-accent/15 px-2 py-0.5 text-[11px] font-semibold uppercase tracking-wide text-accent">
            Recommended
          </span>
        )}
      </span>
      {option.description && <FieldDescription text={option.description} />}
      {option.preview && <OptionPreview text={option.preview} />}
    </span>
  );
}

function optionRowClass(selected: boolean): string {
  return `flex min-h-12 w-full cursor-pointer items-start gap-2.5 rounded-xl border p-3 text-left transition-colors focus-within:ring-2 focus-within:ring-accent ${
    selected
      ? 'border-accent bg-accent/10'
      : 'border-line bg-raised hover:bg-surface active:bg-surface'
  }`;
}

function TextControl({
  field,
  value,
  onChange,
  error,
  disabled,
  autoFocus,
  onShortcut,
}: {
  field: Extract<ElicitationField, { kind: 'text' }>;
  value: string;
  onChange: (value: string) => void;
  error?: string;
  disabled?: boolean;
  autoFocus?: boolean;
  onShortcut?: () => void;
}) {
  const id = useId();
  const [revealed, setRevealed] = useState(false);
  return (
    <div className="min-w-0">
      <FieldHeading field={field} htmlFor={id} />
      {field.description && <FieldDescription text={field.description} />}
      <div className="relative mt-1.5">
        <input
          id={id}
          type={field.secret && !revealed ? 'password' : 'text'}
          value={value}
          onChange={event => onChange(event.target.value)}
          onKeyDown={event => {
            if (event.key === 'Enter' && (event.metaKey || event.ctrlKey) && onShortcut) {
              event.preventDefault();
              onShortcut();
            }
          }}
          autoFocus={autoFocus}
          autoComplete="off"
          disabled={disabled}
          minLength={field.min_length}
          maxLength={field.max_length}
          pattern={field.pattern}
          aria-invalid={Boolean(error)}
          aria-describedby={error ? `${id}-error` : field.description ? `${id}-description` : undefined}
          className={`min-h-11 w-full rounded-lg border bg-bg px-3 py-2 text-[15px] text-fg placeholder:text-muted focus:border-accent ${
            error ? 'border-danger' : 'border-line'
          } ${field.secret ? 'pr-12' : ''}`}
        />
        {field.secret && (
          <button
            type="button"
            onClick={() => setRevealed(current => !current)}
            aria-label={revealed ? 'Hide value' : 'Show value'}
            aria-pressed={revealed}
            disabled={disabled}
            className="absolute inset-y-0 right-0 inline-flex min-w-11 items-center justify-center rounded-r-lg text-muted hover:text-fg"
          >
            {revealed ? <EyeOff className="size-4" aria-hidden /> : <Eye className="size-4" aria-hidden />}
          </button>
        )}
      </div>
      <FieldError id={`${id}-error`} message={error} />
    </div>
  );
}

function NumberControl({
  field,
  value,
  onChange,
  error,
  disabled,
  autoFocus,
  onShortcut,
}: {
  field: Extract<ElicitationField, { kind: 'integer' | 'number' }>;
  value: string;
  onChange: (value: string) => void;
  error?: string;
  disabled?: boolean;
  autoFocus?: boolean;
  onShortcut?: () => void;
}) {
  const id = useId();
  const integer = field.kind === 'integer';
  return (
    <div className="min-w-0">
      <FieldHeading field={field} htmlFor={id} />
      {field.description && <FieldDescription text={field.description} />}
      <input
        id={id}
        type="text"
        inputMode={integer ? 'numeric' : 'decimal'}
        value={value}
        onChange={event => onChange(event.target.value)}
        onKeyDown={event => {
          if (event.key === 'Enter' && (event.metaKey || event.ctrlKey) && onShortcut) {
            event.preventDefault();
            onShortcut();
          }
        }}
        autoFocus={autoFocus}
        autoComplete="off"
        disabled={disabled}
        aria-invalid={Boolean(error)}
        aria-describedby={error ? `${id}-error` : undefined}
        className={`mt-1.5 min-h-11 w-full rounded-lg border bg-bg px-3 py-2 font-mono text-[15px] text-fg placeholder:text-muted focus:border-accent ${
          error ? 'border-danger' : 'border-line'
        }`}
      />
      {(field.minimum != null || field.maximum != null) && (
        <p className="mt-1 text-[13px] text-muted">
          {field.minimum != null && field.maximum != null
            ? `Between ${field.minimum} and ${field.maximum}.`
            : field.minimum != null
              ? `At least ${field.minimum}.`
              : `At most ${field.maximum}.`}
        </p>
      )}
      <FieldError id={`${id}-error`} message={error} />
    </div>
  );
}

function BooleanControl({
  field,
  value,
  onChange,
  error,
  disabled,
  autoFocus,
}: {
  field: Extract<ElicitationField, { kind: 'boolean' }>;
  value: boolean;
  onChange: (value: boolean) => void;
  error?: string;
  disabled?: boolean;
  autoFocus?: boolean;
}) {
  const id = useId();
  return (
    <div className="min-w-0">
      <FieldHeading field={field} />
      {field.description && <FieldDescription text={field.description} />}
      <label htmlFor={id} className={optionRowClass(value) + ' mt-1.5'}>
        <input
          id={id}
          type="checkbox"
          checked={value}
          onChange={event => onChange(event.target.checked)}
          disabled={disabled}
          autoFocus={autoFocus}
          aria-invalid={Boolean(error)}
          className="sr-only"
        />
        <OptionIndicator selected={value} multiple />
        <span className="min-w-0 flex-1 break-words text-[15px] font-medium text-fg">
          {value ? 'Yes' : 'No'}
        </span>
      </label>
      <FieldError id={`${id}-error`} message={error} />
    </div>
  );
}

function ChoiceControl({
  field,
  value,
  onChange,
  error,
  disabled,
  customChoiceTitle,
  autoFocus,
}: {
  field: Extract<ElicitationField, { kind: 'single_select' | 'multi_select' }>;
  value: DraftValue | undefined;
  onChange: (value: DraftValue | undefined) => void;
  error?: string;
  disabled?: boolean;
  customChoiceTitle?: string;
  autoFocus?: boolean;
}) {
  const id = useId();
  const multiple = field.kind === 'multi_select';
  const selectedList = listValue(value);
  const selectedSingle = textValue(value);
  const multiField = field.kind === 'multi_select' ? field : null;
  const minimum = multiField
    ? multiField.required
      ? Math.max(1, multiField.min_items ?? 1)
      : multiField.min_items
    : undefined;
  const maxItems = multiField?.max_items ?? undefined;
  const showCustomChoice = !multiple && customChoiceTitle !== undefined;

  const toggle = (optionValue: string) => {
    if (multiple) {
      const next = selectedList.includes(optionValue)
        ? selectedList.filter(item => item !== optionValue)
        : [...selectedList, optionValue];
      onChange(next);
    } else {
      onChange(optionValue);
    }
  };

  return (
    <fieldset className="min-w-0" aria-describedby={error ? `${id}-error` : undefined}>
      <legend className="min-w-0 break-words text-[15px] font-semibold text-fg">
        {field.title}
        {field.required && (
          <span className="ml-1 text-danger" aria-hidden>
            *
          </span>
        )}
        {field.required && <span className="sr-only"> (required)</span>}
      </legend>
      {field.description && <FieldDescription text={field.description} />}
      {multiple && (minimum != null || maxItems != null) && (
        <p className="mt-1 text-[13px] text-muted">
          {minimum != null && maxItems != null
            ? `Select ${minimum} to ${maxItems}.`
            : minimum != null
              ? `Select at least ${minimum}.`
              : `Select at most ${maxItems}.`}
        </p>
      )}
      <div className="mt-2 space-y-2">
        {field.options.map(option => {
          const selected = multiple
            ? selectedList.includes(option.value)
            : selectedSingle === option.value;
          return (
            <label key={option.value} className={optionRowClass(selected)}>
              <input
                type={multiple ? 'checkbox' : 'radio'}
                name={id}
                value={option.value}
                checked={selected}
                onChange={() => toggle(option.value)}
                disabled={disabled}
                autoFocus={autoFocus && !multiple && selected}
                className="sr-only"
              />
              <OptionIndicator selected={selected} multiple={multiple} />
              <OptionBody option={option} />
            </label>
          );
        })}
        {showCustomChoice && (
          <label className={optionRowClass(selectedSingle === FREE_TEXT_CHOICE)}>
            <input
              type="radio"
              name={id}
              value={FREE_TEXT_CHOICE}
              checked={selectedSingle === FREE_TEXT_CHOICE}
              onChange={() => onChange(FREE_TEXT_CHOICE)}
              disabled={disabled}
              className="sr-only"
            />
            <OptionIndicator selected={selectedSingle === FREE_TEXT_CHOICE} multiple={false} />
            <span className="min-w-0 flex-1 break-words text-[15px] font-medium text-fg">
              {customChoiceTitle || 'Other / Custom answer'}
            </span>
          </label>
        )}
      </div>
      <FieldError id={`${id}-error`} message={error} />
    </fieldset>
  );
}

export function ElicitationFieldControl(props: ElicitationFieldProps) {
  const { field, value, onChange, error, disabled, customChoiceTitle, autoFocus, onShortcut } = props;
  switch (field.kind) {
    case 'text':
      return (
        <TextControl
          field={field}
          value={textValue(value)}
          onChange={next => onChange(next)}
          error={error}
          disabled={disabled}
          autoFocus={autoFocus}
          onShortcut={onShortcut}
        />
      );
    case 'integer':
    case 'number':
      return (
        <NumberControl
          field={field}
          value={textValue(value)}
          onChange={next => onChange(next)}
          error={error}
          disabled={disabled}
          autoFocus={autoFocus}
          onShortcut={onShortcut}
        />
      );
    case 'boolean':
      return (
        <BooleanControl
          field={field}
          value={value === true}
          onChange={next => onChange(next)}
          error={error}
          disabled={disabled}
          autoFocus={autoFocus}
        />
      );
    case 'single_select':
    case 'multi_select':
      return (
        <ChoiceControl
          field={field}
          value={value}
          onChange={onChange}
          error={error}
          disabled={disabled}
          customChoiceTitle={customChoiceTitle}
          autoFocus={autoFocus}
        />
      );
  }
}

/** The auto-growing free-text companion under its select. */
export function CompanionField({
  field,
  value,
  onChange,
  error,
  disabled,
  autoFocus,
  onShortcut,
}: {
  field: ElicitationField;
  value: DraftValue | undefined;
  onChange: (value: string) => void;
  error?: string;
  disabled?: boolean;
  autoFocus?: boolean;
  onShortcut?: () => void;
}) {
  const id = useId();
  return (
    <div className="min-w-0 rounded-xl border border-line bg-surface p-3">
      <label htmlFor={id} className="block min-w-0 break-words text-sm font-semibold text-fg">
        {field.title}
        {field.required && (
          <span className="ml-1 text-danger" aria-hidden>
            *
          </span>
        )}
      </label>
      {field.description && <FieldDescription text={field.description} />}
      <textarea
        id={id}
        value={textValue(value)}
        onChange={event => onChange(event.target.value)}
        onKeyDown={event => {
          if (event.key === 'Enter' && (event.metaKey || event.ctrlKey) && onShortcut) {
            event.preventDefault();
            onShortcut();
          }
        }}
        rows={3}
        autoFocus={autoFocus}
        disabled={disabled}
        autoComplete="off"
        aria-invalid={Boolean(error)}
        aria-describedby={error ? `${id}-error` : undefined}
        className={`mt-1.5 min-h-24 w-full resize-y rounded-lg border bg-bg px-3 py-2 text-[15px] leading-relaxed text-fg placeholder:text-muted focus:border-accent field-sizing-content ${
          error ? 'border-danger' : 'border-line'
        }`}
      />
      <FieldError id={`${id}-error`} message={error} />
      {!error && (
        <p className="mt-1 text-[12px] text-muted">Ctrl/Cmd+Enter submits.</p>
      )}
    </div>
  );
}
