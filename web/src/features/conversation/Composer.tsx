import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ClipboardEvent,
  type DragEvent,
  type KeyboardEvent as ReactKeyboardEvent,
} from 'react';
import {
  AlertTriangle,
  History,
  ImagePlus,
  LoaderCircle,
  Map as MapIcon,
  Paperclip,
  SendHorizontal,
  SlidersHorizontal,
  Square,
  X,
} from 'lucide-react';
import { getClientState, uploadAttachment } from '../../api/client';
import type {
  FileAttachmentRef,
  UploadedAttachment,
  ViewerConfigOption,
  ViewerPromptImage,
  ViewerSession,
} from '../../api/types';
import { useSessionStore } from '../../stores/useSessionStore';
import { useTranscriptStore } from '../../stores/useTranscriptStore';
import { CommandPalette, paletteMatches, type PaletteItem } from './CommandPalette';
import { PromptHistoryModal } from './PromptHistoryModal';
import { flushDraftSave, getPendingDraft, noteServerDraft, scheduleDraftSave, writeDraftNow } from './draftQueue';
import { describeError } from './format';

/**
 * The mobile-safe composer.
 *
 * The textarea is rendered exactly once for the life of a session: focus,
 * keyboard insets, palette state and attachment changes may all re-render it,
 * but nothing unmounts it, so the keyboard never closes under the reader. The
 * dock pads for the keyboard with `pb-keyboard`/`pb-safe` and never resizes
 * `body`.
 */

const MAX_TEXTAREA_HEIGHT_PX = 160;
const MIN_TEXTAREA_HEIGHT_PX = 36;

export function newCommandId(): string {
  try {
    if (typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function') {
      return crypto.randomUUID();
    }
  } catch {
    // `randomUUID` needs a secure context; the fallback below is equivalent here.
  }
  const bytes = new Uint8Array(16);
  if (typeof crypto !== 'undefined' && typeof crypto.getRandomValues === 'function') {
    crypto.getRandomValues(bytes);
  } else {
    for (let index = 0; index < bytes.length; index += 1) {
      bytes[index] = Math.floor(Math.random() * 256);
    }
  }
  return `web-${Array.from(bytes, byte => byte.toString(16).padStart(2, '0')).join('')}`;
}

interface Attachment {
  id: string;
  kind: 'image' | 'file';
  name: string;
  uploading: boolean;
  error?: string;
  previewUrl?: string;
  image?: ViewerPromptImage;
  file?: FileAttachmentRef;
}

function isFileUpload(uploaded: UploadedAttachment): uploaded is { file: FileAttachmentRef } {
  return 'file' in uploaded && Boolean((uploaded as { file?: FileAttachmentRef }).file);
}

function applyUpload(item: Attachment, uploaded: UploadedAttachment): Attachment {
  if (isFileUpload(uploaded)) {
    return { ...item, uploading: false, name: uploaded.file.filename || item.name, file: uploaded.file };
  }
  return {
    ...item,
    uploading: false,
    image: {
      data_base64: '',
      mime_type: uploaded.mime_type,
      width: uploaded.width,
      height: uploaded.height,
      ...(uploaded.attachment ? { attachment: uploaded.attachment } : {}),
    },
  };
}

function revokePreviews(items: readonly Attachment[]): void {
  for (const item of items) {
    if (!item.previewUrl) continue;
    try {
      URL.revokeObjectURL(item.previewUrl);
    } catch {
      // A revoked or unsupported object URL is not a reason to fail a send.
    }
  }
}

function useCoarsePointer(): boolean {
  return useMemo(() => {
    if (typeof window === 'undefined' || typeof window.matchMedia !== 'function') return false;
    try {
      return window.matchMedia('(pointer: coarse)').matches;
    } catch {
      return false;
    }
  }, []);
}

function currentChoiceName(option: ViewerConfigOption): string {
  const choice = option.choices.find(item => String(item.value) === String(option.current));
  return choice?.name || String(option.current ?? '');
}

export interface ComposerProps {
  session: ViewerSession;
  /** Re-attach the transcript to the end after an accepted submission. */
  onFollowBottom?: () => void;
}

export function Composer({ session, onFollowBottom }: ComposerProps) {
  const capabilities = session.capabilities;
  const transitioning = Boolean(session.transitioning || session.verdict.transition);
  const canPrompt = capabilities.open && capabilities.prompt && !transitioning;
  const canShell = capabilities.open && capabilities.run_shell && !transitioning;
  const canType = canPrompt || canShell;
  const coarsePointer = useCoarsePointer();

  // Text the reader left unsent survives leaving and re-entering the session.
  const [text, setTextState] = useState(() => getPendingDraft(session.id) ?? '');
  const textRef = useRef(text);
  // True once the reader (not the server draft) has changed the text.
  const userEdited = useRef(false);
  const setText = useCallback((next: string, fromUser = true) => {
    textRef.current = next;
    if (fromUser) userEdited.current = true;
    setTextState(next);
  }, []);
  const [attachments, setAttachments] = useState<Attachment[]>([]);
  const [submitting, setSubmitting] = useState(false);
  const [dragging, setDragging] = useState(false);
  const submittingRef = useRef(false);
  const [error, setError] = useState<string | null>(null);
  const [paletteDismissed, setPaletteDismissed] = useState(false);
  const [paletteIndex, setPaletteIndex] = useState(0);
  const [configOpen, setConfigOpen] = useState(false);
  const [configPending, setConfigPending] = useState<string | null>(null);
  const [planPending, setPlanPending] = useState(false);
  const [interruptPending, setInterruptPending] = useState<string | null>(null);
  const [historyOpen, setHistoryOpen] = useState(false);

  const textareaRef = useRef<HTMLTextAreaElement | null>(null);
  const imageInputRef = useRef<HTMLInputElement | null>(null);
  const fileInputRef = useRef<HTMLInputElement | null>(null);
  const draftLoaded = useRef(false);

  // Load the server draft once per session open. Text typed while the request
  // is in flight wins: the draft must never overwrite the reader's words.
  // Cleanup flushes unsaved text so leaving the session never loses an edit.
  useEffect(() => {
    const id = session.id;
    draftLoaded.current = false;
    userEdited.current = false;
    setText(getPendingDraft(id) ?? '', false);
    let cancelled = false;
    getClientState(id)
      .then(state => {
        if (cancelled) return;
        const serverDraft = state.draft ?? '';
        draftLoaded.current = true;
        noteServerDraft(id, serverDraft);
        if (userEdited.current) {
          // The reader typed while the draft was loading; their text is newer.
          if (textRef.current !== serverDraft) writeDraftNow(id, textRef.current);
        } else if (getPendingDraft(id) === null && serverDraft && !textRef.current) {
          setText(serverDraft, false);
        }
      })
      .catch(() => {
        // A missing draft is not worth an error; typing will save a new one.
        if (!cancelled) draftLoaded.current = true;
      });
    return () => {
      cancelled = true;
      flushDraftSave(id);
    };
  }, [session.id, setText]);

  useEffect(() => {
    if (!draftLoaded.current && !userEdited.current) return;
    scheduleDraftSave(session.id, text);
  }, [text, session.id]);

  // Grow the textarea in place. CSSOM assignment is allowed under the strict
  // `style-src 'self'` policy; a style attribute never appears in the HTML.
  useLayoutEffect(() => {
    const element = textareaRef.current;
    if (!element) return;
    element.style.height = 'auto';
    const height = Math.min(
      Math.max(element.scrollHeight || MIN_TEXTAREA_HEIGHT_PX, MIN_TEXTAREA_HEIGHT_PX),
      MAX_TEXTAREA_HEIGHT_PX,
    );
    element.style.height = `${height}px`;
  }, [text]);

  const commands = useMemo(() => session.available_commands ?? [], [session.available_commands]);
  const matches = useMemo(
    () => (paletteDismissed ? [] : paletteMatches(text, commands)),
    [text, commands, paletteDismissed],
  );
  const selectedPaletteIndex = matches.length ? Math.min(paletteIndex, matches.length - 1) : 0;

  useEffect(() => {
    setPaletteIndex(0);
  }, [text]);

  const acceptPalette = useCallback(
    (item: PaletteItem) => {
      setText(item.insert);
      setPaletteDismissed(true);
      textareaRef.current?.focus();
    },
    [setText],
  );

  const pickFiles = useCallback(
    async (list: ArrayLike<File> | null, kind: 'image' | 'file') => {
      if (!list || list.length === 0) return;
      for (const file of Array.from(list)) {
        if (kind === 'image' && !session.prompt_images_supported) continue;
        const id = newCommandId();
        let previewUrl: string | undefined;
        if (kind === 'image') {
          try {
            if (typeof URL.createObjectURL === 'function') previewUrl = URL.createObjectURL(file);
          } catch {
            // A thumbnail is an enhancement; the upload still proceeds.
          }
        }
        setAttachments(current => [...current, { id, kind, name: file.name, uploading: true, previewUrl }]);
        try {
          const uploaded = await uploadAttachment(session.id, file, kind);
          setAttachments(current =>
            current.map(item => (item.id === id ? applyUpload(item, uploaded) : item)),
          );
        } catch (failure) {
          setAttachments(current =>
            current.map(item =>
              item.id === id ? { ...item, uploading: false, error: describeError(failure) } : item,
            ),
          );
        }
      }
    },
    [session.id, session.prompt_images_supported],
  );

  /** Pasted and dropped files: images go up as images when the harness takes them, everything else as files. */
  const attachDroppedOrPastedFiles = useCallback(
    async (files: File[]) => {
      const asImages = session.prompt_images_supported;
      const images = asImages ? files.filter(file => file.type.startsWith('image/')) : [];
      const others = asImages ? files.filter(file => !file.type.startsWith('image/')) : files;
      await Promise.all([pickFiles(images, 'image'), pickFiles(others, 'file')]);
    },
    [pickFiles, session.prompt_images_supported],
  );

  const onPaste = (event: ClipboardEvent<HTMLTextAreaElement>) => {
    if (!canPrompt) return;
    const data = event.clipboardData;
    let files = Array.from(data.files);
    if (files.length === 0) {
      files = Array.from(data.items)
        .filter(item => item.kind === 'file')
        .flatMap(item => item.getAsFile() ?? []);
    }
    if (files.length === 0) return;
    // A screenshot arrives with no text; keep the text of a mixed paste.
    if (!data.getData('text/plain')) event.preventDefault();
    void attachDroppedOrPastedFiles(files);
  };

  const carriesFiles = (event: DragEvent<HTMLDivElement>) => event.dataTransfer.types.includes('Files');

  const onDragOver = (event: DragEvent<HTMLDivElement>) => {
    if (!canPrompt || !carriesFiles(event)) return;
    event.preventDefault();
    setDragging(true);
  };

  const onDragLeave = (event: DragEvent<HTMLDivElement>) => {
    if (event.currentTarget.contains(event.relatedTarget as Node | null)) return;
    setDragging(false);
  };

  const onDrop = (event: DragEvent<HTMLDivElement>) => {
    setDragging(false);
    if (!canPrompt || !carriesFiles(event)) return;
    event.preventDefault();
    void attachDroppedOrPastedFiles(Array.from(event.dataTransfer.files));
  };

  const removeAttachment = useCallback((id: string) => {
    setAttachments(current => {
      const target = current.find(item => item.id === id);
      if (!target) return current;
      revokePreviews([target]);
      return current.filter(item => item.id !== id);
    });
  }, []);

  const submit = useCallback(async () => {
    if (submittingRef.current) return;
    const value = textRef.current;
    const items = attachments;
    if (!value.trim() && items.length === 0) return;
    if (items.some(item => item.uploading)) {
      setError('Wait for every attachment to finish uploading.');
      return;
    }
    if (items.some(item => item.error)) {
      setError('Remove the failed attachment before sending.');
      return;
    }
    const shell = value.startsWith('!');
    if (shell) {
      if (!canShell) {
        setError('Shell commands are not available in this session right now.');
        return;
      }
      if (items.length > 0) {
        setError('Shell commands cannot carry attachments.');
        return;
      }
      if (!value.slice(1).trim()) {
        setError('Type the command after “!”.');
        return;
      }
    } else if (!canPrompt) {
      setError('This session cannot take prompts right now.');
      return;
    }

    const commandId = newCommandId();
    submittingRef.current = true;
    setSubmitting(true);
    setError(null);
    // The optimistic row is keyed by the same `command_id` the server echoes,
    // so it is replaced by the real entry instead of duplicating it.
    useTranscriptStore.getState().addOptimisticPrompt(session.id, commandId, value);
    try {
      if (shell) {
        await useSessionStore.getState().dispatchAction({
          action: 'run-shell',
          session_id: session.id,
          command: value.slice(1),
          command_id: commandId,
        });
      } else {
        const images: ViewerPromptImage[] = [];
        const files: FileAttachmentRef[] = [];
        for (const item of items) {
          if (item.image) images.push(item.image);
          if (item.file) files.push(item.file);
        }
        await useSessionStore.getState().dispatchAction({
          action: 'prompt',
          session_id: session.id,
          text: value,
          command_id: commandId,
          ...(images.length ? { images } : {}),
          ...(files.length ? { files } : {}),
        });
      }
      // Clear only what was sent: words typed while the request was in
      // flight belong to the next message and must survive its response.
      if (textRef.current === value) {
        setText('');
        writeDraftNow(session.id, '');
      }
      setAttachments(current => current.filter(item => !items.includes(item)));
      revokePreviews(items);
      onFollowBottom?.();
      textareaRef.current?.focus();
    } catch (failure) {
      useTranscriptStore.getState().removePendingPrompt(session.id, commandId);
      setError(describeError(failure));
    } finally {
      submittingRef.current = false;
      setSubmitting(false);
    }
  }, [attachments, canPrompt, canShell, onFollowBottom, session.id, setText]);

  const togglePlanMode = useCallback(async () => {
    if (planPending) return;
    setPlanPending(true);
    setError(null);
    try {
      await useSessionStore.getState().dispatchAction({
        action: 'set-plan-mode',
        session_id: session.id,
        active: !session.plan_mode_active,
      });
    } catch (failure) {
      setError(describeError(failure));
    } finally {
      setPlanPending(false);
    }
  }, [planPending, session.id, session.plan_mode_active]);

  const chooseConfig = useCallback(
    async (option: ViewerConfigOption, value: string) => {
      if (configPending) return;
      setConfigPending(option.key);
      setError(null);
      try {
        await useSessionStore.getState().dispatchAction({
          action: 'set-config',
          session_id: session.id,
          key: option.key,
          value,
        });
        setConfigOpen(false);
      } catch (failure) {
        setError(describeError(failure));
      } finally {
        setConfigPending(null);
      }
    },
    [configPending, session.id],
  );

  const runInterrupt = useCallback(
    async (key: string, action: () => Promise<unknown>) => {
      if (interruptPending) return;
      setInterruptPending(key);
      setError(null);
      try {
        await action();
      } catch (failure) {
        setError(describeError(failure));
      } finally {
        setInterruptPending(null);
      }
    },
    [interruptPending],
  );

  const onKeyDown = (event: ReactKeyboardEvent<HTMLTextAreaElement>) => {
    if (matches.length > 0) {
      if (event.key === 'ArrowDown') {
        event.preventDefault();
        setPaletteIndex(index => (index + 1) % matches.length);
        return;
      }
      if (event.key === 'ArrowUp') {
        event.preventDefault();
        setPaletteIndex(index => (index - 1 + matches.length) % matches.length);
        return;
      }
      if (event.key === 'Tab' || (event.key === 'Enter' && !event.shiftKey && !event.metaKey && !event.ctrlKey)) {
        event.preventDefault();
        acceptPalette(matches[selectedPaletteIndex]);
        return;
      }
      if (event.key === 'Escape') {
        event.preventDefault();
        setPaletteDismissed(true);
        return;
      }
    }
    if (event.key !== 'Enter') return;
    // Cmd/Ctrl+Enter sends on every device, including phones.
    if (event.metaKey || event.ctrlKey) {
      event.preventDefault();
      void submit();
      return;
    }
    if (event.shiftKey) return; // newline
    // A touch keyboard's Enter inserts a newline; the Send button submits.
    if (coarsePointer) return;
    event.preventDefault();
    void submit();
  };

  const policyKeys = useMemo(() => new Set(session.policy_config_keys ?? []), [session.policy_config_keys]);
  const configOptions = (session.config_options ?? []).filter(option => !policyKeys.has(option.key));
  const modelOption = configOptions.find(option => option.key === 'model');
  const configLabel = modelOption ? currentChoiceName(modelOption) : 'Config';
  const canConfig = Boolean(capabilities.set_config) && !transitioning && configOptions.length > 0;

  const interrupt = Boolean(capabilities.interrupt_turn) && session.verdict.interrupt;
  const interruptAll = Boolean(capabilities.interrupt_all) && session.verdict.interrupt_all;
  const cancelOperation = Boolean(capabilities.cancel_operation) && Boolean(session.operation?.cancellable);
  const showActions = interrupt || interruptAll || cancelOperation;

  const hasContent = text.trim().length > 0 || attachments.length > 0;
  const uploading = attachments.some(item => item.uploading);
  const sendDisabled =
    !canType || submitting || !hasContent || uploading || (text.startsWith('!') && !canShell);
  const shellMode = text.startsWith('!');
  const placeholder = !capabilities.open
    ? 'This session is not live.'
    : transitioning
      ? 'This session is transitioning…'
      : canPrompt
        ? 'Message the agent…'
        : canShell
          ? 'Shell command (!command)…'
          : 'This session cannot take messages.';

  return (
    <div className="shrink-0 border-t border-line bg-surface pb-keyboard pb-safe" data-testid="composer">
      <div className="mx-auto w-full max-w-3xl px-3 py-2">
        {error && (
          <div
            role="alert"
            className="mb-2 flex items-start gap-2 rounded-lg border border-danger/40 bg-danger/10 px-2.5 py-1.5 text-[12px] text-danger"
          >
            <AlertTriangle className="mt-0.5 size-3.5 shrink-0" aria-hidden />
            <span className="min-w-0 flex-1 break-words">{error}</span>
            <button type="button" aria-label="Dismiss error" onClick={() => setError(null)}>
              <X className="size-3.5" aria-hidden />
            </button>
          </div>
        )}

        {!canType && (
          <p className="mb-2 rounded-lg border border-line bg-bg/60 px-2.5 py-1.5 text-[11.5px] text-muted" role="status">
            {placeholder}
          </p>
        )}

        {showActions && (
          <div className="mb-2 flex flex-wrap items-center gap-1.5">
            {interrupt && (
              <button
                type="button"
                disabled={interruptPending !== null}
                onClick={() =>
                  void runInterrupt('interrupt', () =>
                    useSessionStore.getState().dispatchAction({
                      action: 'interrupt-turn',
                      session_id: session.id,
                    }),
                  )
                }
                className="inline-flex min-h-8 items-center gap-1.5 rounded-full border border-warn/50 bg-warn/10 px-2.5 py-1 text-[11.5px] font-medium text-warn disabled:opacity-60"
              >
                {interruptPending === 'interrupt' ? (
                  <LoaderCircle className="size-3.5 animate-spin" aria-hidden />
                ) : (
                  <Square className="size-3" aria-hidden />
                )}
                Interrupt
              </button>
            )}
            {interruptAll && (
              <button
                type="button"
                disabled={interruptPending !== null}
                onClick={() =>
                  void runInterrupt('interrupt-all', () =>
                    useSessionStore.getState().dispatchAction({
                      action: 'interrupt-all',
                      session_id: session.id,
                    }),
                  )
                }
                className="inline-flex min-h-8 items-center gap-1.5 rounded-full border border-danger/50 bg-danger/10 px-2.5 py-1 text-[11.5px] font-medium text-danger disabled:opacity-60"
              >
                {interruptPending === 'interrupt-all' ? (
                  <LoaderCircle className="size-3.5 animate-spin" aria-hidden />
                ) : (
                  <Square className="size-3" aria-hidden />
                )}
                Interrupt all
              </button>
            )}
            {cancelOperation && (
              <button
                type="button"
                disabled={interruptPending !== null}
                onClick={() =>
                  void runInterrupt('cancel', () =>
                    useSessionStore.getState().dispatchAction({
                      action: 'cancel',
                      session_id: session.id,
                    }),
                  )
                }
                className="inline-flex min-h-8 items-center gap-1.5 rounded-full border border-line px-2.5 py-1 text-[11.5px] text-muted hover:text-fg disabled:opacity-60"
              >
                {interruptPending === 'cancel' ? (
                  <LoaderCircle className="size-3.5 animate-spin" aria-hidden />
                ) : (
                  <X className="size-3.5" aria-hidden />
                )}
                Cancel {session.operation?.kind ?? 'operation'}
              </button>
            )}
          </div>
        )}

        {attachments.length > 0 && (
          <ul className="mb-2 flex flex-wrap gap-1.5" aria-label="Attachments">
            {attachments.map(item => (
              <li
                key={item.id}
                className="inline-flex max-w-full items-center gap-1.5 rounded-full border border-line bg-raised py-1 pl-1.5 pr-1 text-[11px]"
              >
                {item.previewUrl ? (
                  <img src={item.previewUrl} alt="" className="size-5 shrink-0 rounded object-cover" />
                ) : (
                  <Paperclip className="size-3.5 shrink-0 text-muted" aria-hidden />
                )}
                <span className="max-w-40 truncate text-fg">{item.name}</span>
                {item.uploading && <LoaderCircle className="size-3 animate-spin text-muted" aria-label="Uploading" />}
                {item.error && (
                  <span className="text-danger" title={item.error}>
                    failed
                  </span>
                )}
                <button
                  type="button"
                  aria-label={`Remove ${item.name}`}
                  onClick={() => removeAttachment(item.id)}
                  className="grid size-5 shrink-0 place-items-center rounded-full text-muted hover:bg-surface hover:text-fg"
                >
                  <X className="size-3" aria-hidden />
                </button>
              </li>
            ))}
          </ul>
        )}

        <div className="relative">
          {matches.length > 0 && (
            <CommandPalette
              items={matches}
              selected={selectedPaletteIndex}
              onHover={setPaletteIndex}
              onPick={acceptPalette}
            />
          )}
          {configOpen && configOptions.length > 0 && (
            <div className="absolute inset-x-0 bottom-full z-30 mb-1 max-h-72 overflow-y-auto overscroll-contain rounded-xl border border-line bg-surface p-1 shadow-xl">
              {configOptions.map(option => (
                <section key={option.key} className="py-1">
                  <p className="px-2 pb-0.5 text-[10.5px] uppercase tracking-wide text-muted">{option.label}</p>
                  <ul>
                    {option.choices.map(choice => {
                      const chosen = String(choice.value) === String(option.current);
                      return (
                        <li key={String(choice.value)}>
                          <button
                            type="button"
                            onClick={() => void chooseConfig(option, String(choice.value))}
                            disabled={configPending !== null || chosen}
                            aria-pressed={chosen}
                            className={`flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-[12.5px] ${
                              chosen ? 'bg-accent/10 text-accent' : 'text-fg hover:bg-raised'
                            } disabled:opacity-100`}
                          >
                            <span className="min-w-0 flex-1 truncate">{choice.name || String(choice.value)}</span>
                            {configPending === option.key && (
                              <LoaderCircle className="size-3.5 animate-spin" aria-hidden />
                            )}
                          </button>
                        </li>
                      );
                    })}
                  </ul>
                </section>
              ))}
              {policyKeys.size > 0 && (
                <p className="border-t border-line px-2 py-1 text-[10.5px] text-muted">
                  Managed by policy: {[...policyKeys].join(', ')}
                </p>
              )}
            </div>
          )}

          <div
            className={`flex items-end gap-1.5 rounded-xl border bg-bg p-1.5 ${dragging ? 'border-accent' : 'border-line'}`}
            onDragOver={onDragOver}
            onDragLeave={onDragLeave}
            onDrop={onDrop}
          >
            {canPrompt && (
              <div className="flex shrink-0 items-center gap-0.5 self-end pb-0.5">
                {session.prompt_images_supported && (
                  <>
                    <input
                      ref={imageInputRef}
                      type="file"
                      accept="image/*"
                      multiple
                      className="hidden"
                      aria-hidden="true"
                      tabIndex={-1}
                      onChange={event => {
                        void pickFiles(event.currentTarget.files, 'image');
                        event.currentTarget.value = '';
                      }}
                    />
                    <button
                      type="button"
                      aria-label="Attach image"
                      onClick={() => imageInputRef.current?.click()}
                      className="grid size-8 place-items-center rounded-lg text-muted hover:bg-raised hover:text-fg"
                    >
                      <ImagePlus className="size-4" aria-hidden />
                    </button>
                  </>
                )}
                <>
                  <input
                    ref={fileInputRef}
                    type="file"
                    multiple
                    className="hidden"
                    aria-hidden="true"
                    tabIndex={-1}
                    onChange={event => {
                      void pickFiles(event.currentTarget.files, 'file');
                      event.currentTarget.value = '';
                    }}
                  />
                  <button
                    type="button"
                    aria-label="Attach file"
                    onClick={() => fileInputRef.current?.click()}
                    className="grid size-8 place-items-center rounded-lg text-muted hover:bg-raised hover:text-fg"
                  >
                    <Paperclip className="size-4" aria-hidden />
                  </button>
                </>
              </div>
            )}
            <textarea
              ref={textareaRef}
              rows={1}
              value={text}
              disabled={!canType}
              onChange={event => {
                setText(event.target.value);
                setPaletteDismissed(false);
                setError(null);
              }}
              onKeyDown={onKeyDown}
              onPaste={onPaste}
              placeholder={placeholder}
              aria-label="Message"
              className="min-h-9 max-h-40 min-w-0 flex-1 resize-none bg-transparent px-1 py-2 text-[15px] leading-snug text-fg outline-none placeholder:text-muted disabled:opacity-60"
            />
            <button
              type="button"
              aria-label="Send"
              onClick={() => void submit()}
              disabled={sendDisabled}
              className="grid size-9 shrink-0 self-end place-items-center rounded-lg bg-accent text-accent-fg disabled:bg-raised disabled:text-muted"
            >
              {submitting ? (
                <LoaderCircle className="size-4 animate-spin" aria-hidden />
              ) : (
                <SendHorizontal className="size-4" aria-hidden />
              )}
            </button>
          </div>
        </div>

        <div className="mt-1.5 flex flex-wrap items-center gap-1.5">
          {capabilities.set_plan_mode && (
            <button
              type="button"
              aria-pressed={Boolean(session.plan_mode_active)}
              aria-label={session.plan_mode_active ? 'Disable plan mode' : 'Enable plan mode'}
              disabled={planPending || !canType}
              onClick={() => void togglePlanMode()}
              className={`inline-flex min-h-7 items-center gap-1.5 rounded-full border px-2.5 py-0.5 text-[11px] font-medium disabled:opacity-60 ${
                session.plan_mode_active
                  ? 'border-accent bg-accent/15 text-accent'
                  : 'border-line text-muted hover:text-fg'
              }`}
            >
              {planPending ? (
                <LoaderCircle className="size-3 animate-spin" aria-hidden />
              ) : (
                <MapIcon className="size-3" aria-hidden />
              )}
              Plan mode
            </button>
          )}
          {canConfig && (
            <button
              type="button"
              aria-label="Model and configuration"
              aria-expanded={configOpen}
              onClick={() => setConfigOpen(open => !open)}
              className="inline-flex min-h-7 max-w-56 items-center gap-1.5 rounded-full border border-line px-2.5 py-0.5 text-[11px] text-muted hover:text-fg"
            >
              <SlidersHorizontal className="size-3 shrink-0" aria-hidden />
              <span className="truncate">{configLabel}</span>
            </button>
          )}
          <button
            type="button"
            onClick={() => setHistoryOpen(true)}
            className="inline-flex min-h-7 items-center gap-1.5 rounded-full border border-line px-2.5 py-0.5 text-[11px] text-muted hover:text-fg"
          >
            <History className="size-3" aria-hidden />
            History
          </button>
          <span className="ml-auto text-[10.5px] text-muted">
            {shellMode
              ? canShell
                ? 'Shell command · runs in this session'
                : 'Shell commands are unavailable'
              : coarsePointer
                ? 'Tap Send to submit · Enter adds a line'
                : 'Enter to send · Shift+Enter for a new line'}
          </span>
        </div>
      </div>

      {historyOpen && (
        <PromptHistoryModal
          sessionId={session.id}
          onClose={() => setHistoryOpen(false)}
          onPick={prompt => setText(prompt)}
        />
      )}
    </div>
  );
}
