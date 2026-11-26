// Everything the elicitation UI decides about an answer.
//
// The rules mirror `mj-core/src/elicitation.rs` exactly:
// - A select's companion is the FIRST text field whose `custom_answer_for`
//   names it (`ElicitationRequest::companion`).
// - `custom_answer_option` absent means Alternative (the text answers in place
//   of a pick), `\u001falongside` means Alongside (text rides beside a pick, or
//   answers alone), and any other value means Detail (text belongs to that one
//   option and is sent with it).
// - Only fields the person actually answered are sent. A required field that
//   the request leaves unanswered is refused here before it reaches the daemon.
//
// The exported functions are pure so tests can pin the wire payloads.

import type {
  ElicitationField,
  ElicitationOption,
  ElicitationRequest,
  ElicitationResponse,
  ElicitationValue,
} from '../../api/types';
import { ALONGSIDE_OPTION } from '../../api/types';

/** The radio value of a single-select's free-text alternative; it picks no option. */
export const FREE_TEXT_CHOICE = '\u001ffree-text';

/** What one field holds while the person is answering. Numbers stay as input text. */
export type DraftValue = string | boolean | string[];

export type DraftValues = Record<string, DraftValue>;

export type CompanionRole = 'alternative' | 'alongside' | 'detail';

export interface LogicalQuestion {
  field: ElicitationField;
  companion?: ElicitationField;
  role?: CompanionRole;
}

export interface FieldErrors {
  [fieldId: string]: string;
}

/** How a companion text field takes part in its select's answer. */
export function companionRole(field: ElicitationField): CompanionRole {
  if (field.custom_answer_option == null) return 'alternative';
  if (field.custom_answer_option === ALONGSIDE_OPTION) return 'alongside';
  return 'detail';
}

/**
 * Group companion fields under the select they answer. The first text field
 * naming a select is its companion, exactly as `ElicitationRequest::companion`
 * decides it. A companion of a missing parent stands on its own.
 */
export function buildQuestions(request: ElicitationRequest): LogicalQuestion[] {
  const fields = request.fields ?? [];
  const companionIndex = new Map<number, number>();
  for (let index = 0; index < fields.length; index += 1) {
    const parent = fields[index];
    if (parent.kind !== 'single_select' && parent.kind !== 'multi_select') continue;
    const found = fields.findIndex(
      candidate => candidate.kind === 'text' && candidate.custom_answer_for === parent.id,
    );
    if (found >= 0 && ![...companionIndex.values()].includes(found)) companionIndex.set(index, found);
  }
  const consumed = new Set(companionIndex.values());
  const questions: LogicalQuestion[] = [];
  for (let index = 0; index < fields.length; index += 1) {
    if (consumed.has(index)) continue;
    const field = fields[index];
    const companionIndexValue = companionIndex.get(index);
    if (companionIndexValue === undefined) {
      questions.push({ field });
      continue;
    }
    const companion = fields[companionIndexValue];
    questions.push({ field, companion, role: companionRole(companion) });
  }
  return questions;
}

/** The default a field starts with, matching the server's parsed schema. */
export function defaultValueFor(field: ElicitationField): DraftValue | undefined {
  switch (field.kind) {
    case 'text':
      return field.default ?? '';
    case 'single_select':
      // The Rust parser fills a select's default from the recommended option,
      // else the first one; a required select therefore always has an answer.
      return field.default ?? (field.required ? field.options[0]?.value : undefined);
    case 'multi_select':
      return [...(field.default ?? [])];
    case 'boolean':
      return field.default === true;
    case 'integer':
    case 'number':
      return field.default == null ? '' : String(field.default);
  }
}

/** Start values for every field, including companions. */
export function initialValues(request: ElicitationRequest): DraftValues {
  const values: DraftValues = {};
  for (const question of buildQuestions(request)) {
    values[question.field.id] = defaultValueFor(question.field) ?? '';
    if (question.companion) values[question.companion.id] = '';
  }
  return values;
}

/** Keep only values the request still has, then fill missing defaults. */
export function mergeDraftValues(
  request: ElicitationRequest,
  stored: DraftValues | undefined,
): DraftValues {
  const values = initialValues(request);
  if (!stored) return values;
  const known = new Set<string>();
  for (const question of buildQuestions(request)) {
    known.add(question.field.id);
    if (question.companion) known.add(question.companion.id);
  }
  for (const [id, value] of Object.entries(stored)) {
    if (!known.has(id)) continue;
    if (typeof value === 'string' || typeof value === 'boolean' || Array.isArray(value)) {
      values[id] = Array.isArray(value) ? value.filter(item => typeof item === 'string') : value;
    }
  }
  return values;
}

export function textValue(value: DraftValue | undefined): string {
  return typeof value === 'string' ? value : '';
}

export function listValue(value: DraftValue | undefined): string[] {
  return Array.isArray(value) ? value.filter((item): item is string => typeof item === 'string') : [];
}

function selectedOptions(field: ElicitationField): ElicitationOption[] {
  return field.kind === 'single_select' || field.kind === 'multi_select' ? field.options : [];
}

/** Whether the companion's option is the current pick. */
export function isCompanionVisible(question: LogicalQuestion, values: DraftValues): boolean {
  const { field, companion, role } = question;
  if (!companion || !role) return false;
  if (role === 'alongside') return true;
  if (role === 'alternative') {
    if (field.kind === 'single_select') return values[field.id] === FREE_TEXT_CHOICE;
    // A multi-select has no "Other" row: its text answers as soon as text exists.
    return true;
  }
  const expected = companion.custom_answer_option;
  if (expected == null) return false;
  if (field.kind === 'single_select') return values[field.id] === expected;
  if (field.kind === 'multi_select') return listValue(values[field.id]).includes(expected);
  return false;
}

/** Whether the companion text is the answer in place of the parent's picks. */
export function alternativeAnswers(question: LogicalQuestion, values: DraftValues): boolean {
  if (question.role !== 'alternative' || !question.companion) return false;
  if (question.field.kind === 'single_select') return values[question.field.id] === FREE_TEXT_CHOICE;
  return textValue(values[question.companion.id]).trim() !== '';
}

/** Text is trimmed for ordinary fields; secrets stay byte-for-byte. */
function normalizedText(field: ElicitationField, value: string): string {
  return field.secret ? value : value.trim();
}

function validateText(field: ElicitationField, value: string): string | null {
  const text = normalizedText(field, value);
  const length = [...text].length;
  if (field.kind !== 'text') return null;
  if (field.min_length != null && length < field.min_length) {
    return `${field.title} must be at least ${field.min_length} character${field.min_length === 1 ? '' : 's'}.`;
  }
  if (field.max_length != null && length > field.max_length) {
    return `${field.title} must be at most ${field.max_length} character${field.max_length === 1 ? '' : 's'}.`;
  }
  if (field.pattern) {
    let matches = true;
    try {
      matches = new RegExp(field.pattern).test(text);
    } catch {
      // The daemon validated the pattern; an unsupported construct must not
      // block an answer the harness can still accept.
      matches = true;
    }
    if (!matches) return `${field.title} does not match the required format.`;
  }
  switch (field.format) {
    case 'email': {
      const at = text.indexOf('@');
      const local = at >= 0 ? text.slice(0, at) : '';
      const domain = at >= 0 ? text.slice(at + 1) : '';
      if (!local || !domain.includes('.') || domain.startsWith('.')) {
        return `${field.title} must be an email address.`;
      }
      break;
    }
    case 'uri':
      try {
        // eslint-disable-next-line no-new
        new URL(text);
      } catch {
        return `${field.title} must be a URI.`;
      }
      break;
    case 'date':
      if (!/^\d{4}-\d{2}-\d{2}$/.test(text) || Number.isNaN(Date.parse(`${text}T00:00:00Z`))) {
        return `${field.title} must be a YYYY-MM-DD date.`;
      }
      break;
    case 'date-time':
      if (!/^\d{4}-\d{2}-\d{2}T/.test(text) || Number.isNaN(Date.parse(text))) {
        return `${field.title} must be an RFC 3339 date-time.`;
      }
      break;
    default:
      break;
  }
  return null;
}

function validateNumber(
  field: ElicitationField,
  raw: string,
  integer: boolean,
): { value?: number; error?: string } {
  const text = raw.trim();
  if (!text) return {};
  if (integer && !/^[+-]?\d+$/.test(text)) {
    return { error: `${field.title} must be a whole number.` };
  }
  const value = Number(text);
  if (!Number.isFinite(value)) return { error: `${field.title} must be a number.` };
  if (integer && !Number.isInteger(value)) return { error: `${field.title} must be a whole number.` };
  if (
    (field.kind === 'integer' || field.kind === 'number') &&
    ((field.minimum != null && value < field.minimum) ||
      (field.maximum != null && value > field.maximum))
  ) {
    const range =
      field.minimum != null && field.maximum != null
        ? `between ${field.minimum} and ${field.maximum}`
        : field.minimum != null
          ? `at least ${field.minimum}`
          : `at most ${field.maximum}`;
    return { error: `${field.title} must be ${range}.` };
  }
  return { value };
}

function invalidOption(field: ElicitationField): string {
  return `${field.title} is not one of the offered options.`;
}

function validateParent(question: LogicalQuestion, values: DraftValues): FieldErrors {
  const field = question.field;
  const errors: FieldErrors = {};
  const value = values[field.id];
  switch (field.kind) {
    case 'text': {
      const text = typeof value === 'string' ? normalizedText(field, value) : '';
      if (!text) {
        if (field.required) errors[field.id] = `${field.title} is required.`;
      } else {
        const message = validateText(field, typeof value === 'string' ? value : '');
        if (message) errors[field.id] = message;
      }
      break;
    }
    case 'single_select': {
      const selected = typeof value === 'string' ? value : '';
      if (!selected) {
        if (field.required) errors[field.id] = `${field.title} is required.`;
      } else if (!selectedOptions(field).some(option => option.value === selected)) {
        errors[field.id] = invalidOption(field);
      }
      break;
    }
    case 'multi_select': {
      const selected = listValue(value);
      const minimum = field.required ? Math.max(1, field.min_items ?? 1) : field.min_items;
      if (selected.length === 0) {
        if (field.required) errors[field.id] = `${field.title} is required.`;
      } else {
        if (minimum != null && selected.length < minimum) {
          errors[field.id] = `${field.title} needs at least ${minimum} selection${minimum === 1 ? '' : 's'}.`;
        } else if (field.max_items != null && selected.length > field.max_items) {
          errors[field.id] = `${field.title} allows at most ${field.max_items} selection${field.max_items === 1 ? '' : 's'}.`;
        } else if (!selected.every(item => selectedOptions(field).some(option => option.value === item))) {
          errors[field.id] = invalidOption(field);
        }
      }
      break;
    }
    case 'boolean':
      if (typeof value !== 'boolean') errors[field.id] = `${field.title} is required.`;
      break;
    case 'integer':
    case 'number': {
      if (typeof value !== 'string' || value.trim() === '') {
        if (field.required) errors[field.id] = `${field.title} is required.`;
        break;
      }
      const result = validateNumber(field, value, field.kind === 'integer');
      if (result.error) errors[field.id] = result.error;
      break;
    }
  }
  return errors;
}

function validateCompanion(question: LogicalQuestion, values: DraftValues): FieldErrors {
  const { companion } = question;
  const errors: FieldErrors = {};
  if (!companion) return errors;
  if (!isCompanionVisible(question, values)) return errors;
  const value = values[companion.id];
  const text = typeof value === 'string' ? normalizedText(companion, value) : '';
  const alternative = alternativeAnswers(question, values);
  const parentMissing = (() => {
    const parent = question.field;
    if (parent.kind === 'single_select') return !values[parent.id];
    if (parent.kind === 'multi_select') return listValue(values[parent.id]).length === 0;
    return false;
  })();
  if (alternative && !text) {
    errors[companion.id] = `${companion.title} is required when it answers the question.`;
    return errors;
  }
  if (
    question.role === 'alongside' &&
    parentMissing &&
    question.field.required &&
    !text
  ) {
    errors[question.field.id] = `${question.field.title} is required.`;
    return errors;
  }
  if (text) {
    if (companion.kind === 'text') {
      const message = validateText(companion, text);
      if (message) errors[companion.id] = message;
    }
  } else if (companion.required && !parentMissing) {
    errors[companion.id] = `${companion.title} is required.`;
  }
  return errors;
}

/**
 * Validate one logical question. Alongside text satisfies a required select on
 * its own, matching `ElicitationRequest::harness_response`; a text alternative
 * replaces the picks entirely, so the select's own item limits do not apply.
 */
export function validateQuestion(question: LogicalQuestion, values: DraftValues): FieldErrors {
  const errors: FieldErrors = { ...validateParent(question, values), ...validateCompanion(question, values) };
  const alternative = alternativeAnswers(question, values);
  const companionValue = question.companion ? values[question.companion.id] : undefined;
  const companionText =
    question.companion && typeof companionValue === 'string'
      ? normalizedText(question.companion, companionValue)
      : '';
  const parentEmpty =
    question.field.kind === 'single_select'
      ? !textValue(values[question.field.id]) || values[question.field.id] === FREE_TEXT_CHOICE
      : question.field.kind === 'multi_select'
        ? listValue(values[question.field.id]).length === 0
        : false;
  if (
    alternative ||
    (question.role === 'alongside' && parentEmpty && companionText !== '')
  ) {
    // The text is the answer; only its own presence matters.
    delete errors[question.field.id];
  }
  return errors;
}

/** Validate every logical question; the map is empty when the form is ready. */
export function validateRequest(request: ElicitationRequest, values: DraftValues): FieldErrors {
  const errors: FieldErrors = {};
  for (const question of buildQuestions(request)) {
    Object.assign(errors, validateQuestion(question, values));
  }
  return errors;
}

/** The index of the first logical question with an error. */
export function firstInvalidStep(request: ElicitationRequest, values: DraftValues): number | null {
  const questions = buildQuestions(request);
  for (let index = 0; index < questions.length; index += 1) {
    if (Object.keys(validateQuestion(questions[index], values)).length) return index;
  }
  return null;
}

function answeredParent(question: LogicalQuestion, values: DraftValues): boolean {
  if (alternativeAnswers(question, values)) return true;
  const field = question.field;
  const value = values[field.id];
  switch (field.kind) {
    case 'text':
      return typeof value === 'string' && normalizedText(field, value) !== '';
    case 'single_select':
      return typeof value === 'string' && value !== '' && value !== FREE_TEXT_CHOICE;
    case 'multi_select':
      return listValue(value).length > 0;
    case 'boolean':
      return typeof value === 'boolean';
    case 'integer':
    case 'number':
      return typeof value === 'string' && value.trim() !== '';
  }
}

/** Whether a question has any answer, for the "N unanswered" progress line. */
export function isAnswered(question: LogicalQuestion, values: DraftValues): boolean {
  if (answeredParent(question, values)) return true;
  const companion = question.companion;
  if (!companion || !isCompanionVisible(question, values)) return false;
  return textValue(values[companion.id]).trim() !== '';
}

/**
 * Build the ACP content map. Every participating field sends its own value;
 * the daemon decides how a pick and its companion combine. Fields the person
 * left unanswered are omitted so suggested defaults never leak in.
 */
export function buildContent(
  request: ElicitationRequest,
  values: DraftValues,
): Record<string, ElicitationValue> {
  const content: Record<string, ElicitationValue> = {};
  for (const question of buildQuestions(request)) {
    const { field, companion, role } = question;
    const alternative = alternativeAnswers(question, values);
    const companionValue = companion ? values[companion.id] : undefined;
    const companionText =
      companion && typeof companionValue === 'string'
        ? normalizedText(companion, companionValue)
        : '';
    const companionVisible = companion ? isCompanionVisible(question, values) : false;

    if (field.kind === 'single_select' || field.kind === 'multi_select') {
      const multi = field.kind === 'multi_select';
      const selected = multi ? listValue(values[field.id]) : textValue(values[field.id]);
      const empty = multi
        ? (selected as string[]).length === 0
        : selected === '' || selected === FREE_TEXT_CHOICE;
      if (alternative) {
        if (companionText) content[companion!.id] = companionText;
        continue;
      }
      const alongsideAnswersAlone =
        role === 'alongside' && empty && companionText !== '';
      if (!alongsideAnswersAlone && !(empty && !field.required)) {
        if (multi) content[field.id] = selected as string[];
        else if (selected) content[field.id] = selected;
      }
      if (companionVisible && companionText) content[companion!.id] = companionText;
      continue;
    }

    switch (field.kind) {
      case 'text': {
        const text = typeof values[field.id] === 'string' ? values[field.id] as string : '';
        const normalized = normalizedText(field, text);
        if (normalized) content[field.id] = normalized;
        break;
      }
      case 'boolean':
        content[field.id] = values[field.id] === true;
        break;
      case 'integer':
      case 'number': {
        const raw = textValue(values[field.id]);
        if (raw.trim() !== '') {
          const result = validateNumber(field, raw, field.kind === 'integer');
          if (result.value !== undefined) content[field.id] = result.value;
        }
        break;
      }
    }
  }
  return content;
}

/** The exact `{ action: "accept" }` response sent for these answers. */
export function acceptResponse(
  request: ElicitationRequest,
  values: DraftValues,
): ElicitationResponse {
  const content = buildContent(request, values);
  return Object.keys(content).length
    ? { action: 'accept', content }
    : { action: 'accept' };
}

/** Short plain-text preview for the dock; Markdown markers are not useful there. */
export function previewText(text: string, limit = 180): string {
  const plain = text
    .replace(/```[\s\S]*?```/g, ' ')
    .replace(/`([^`]*)`/g, '$1')
    .replace(/!\[[^\]]*\]\([^)]*\)/g, ' ')
    .replace(/\[([^\]]*)\]\([^)]*\)/g, '$1')
    .replace(/[#>*_~]/g, '')
    .replace(/\s+/g, ' ')
    .trim();
  if (plain.length <= limit) return plain;
  return `${plain.slice(0, Math.max(0, limit - 1)).trimEnd()}…`;
}

/** Split a title's "(Recommended)" marker into a badge, if present. */
export function splitRecommended(title: string): { title: string; recommended: boolean } {
  const match = /\s*[([{]\s*recommended\s*[)\]}]/i.exec(title);
  if (!match) return { title, recommended: false };
  return {
    title: `${title.slice(0, match.index)}${title.slice(match.index + match[0].length)}`.trim(),
    recommended: true,
  };
}
