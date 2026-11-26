import { X } from 'lucide-react';
import { useEffect } from 'react';
import { useUiStore, type Toast } from '../../stores/useUiStore';

const TONE: Record<Toast['kind'], string> = {
  info: 'border-line',
  success: 'border-ok/60',
  error: 'border-danger/70',
};

function ToastItem({ toast }: { toast: Toast }) {
  const dismiss = useUiStore(state => state.dismissToast);
  useEffect(() => {
    // Errors stay longer: they usually say what to do next.
    const timer = setTimeout(() => dismiss(toast.id), toast.kind === 'error' ? 9000 : 5000);
    return () => clearTimeout(timer);
  }, [dismiss, toast.id, toast.kind]);
  return (
    <li
      role={toast.kind === 'error' ? 'alert' : 'status'}
      className={`pointer-events-auto flex items-start gap-2 rounded-xl border bg-raised px-3.5 py-3 text-sm shadow-lg ${TONE[toast.kind]}`}
    >
      <span className="min-w-0 flex-1 break-words">{toast.text}</span>
      <button
        type="button"
        aria-label="Dismiss"
        onClick={() => dismiss(toast.id)}
        className="-m-1 grid size-7 shrink-0 place-items-center rounded-md text-muted hover:text-fg"
      >
        <X className="size-4" aria-hidden />
      </button>
    </li>
  );
}

export function ToastContainer() {
  const toasts = useUiStore(state => state.toasts);
  return (
    <ol
      aria-label="Notices"
      className="pointer-events-none fixed inset-x-0 bottom-0 z-50 mx-auto flex max-w-md flex-col gap-2 px-3 pb-[calc(env(safe-area-inset-bottom)+4.5rem)] md:pb-6"
    >
      {toasts.map(toast => (
        <ToastItem key={toast.id} toast={toast} />
      ))}
    </ol>
  );
}
