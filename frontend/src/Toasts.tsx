import { useCallback, useEffect, useRef, useState } from "react";

export type Toast = {
  id: number;
  message: string;
  tone: "info" | "success" | "warning";
};

export type ToastsApi = {
  toasts: Toast[];
  show: (message: string, tone?: Toast["tone"]) => void;
  dismiss: (id: number) => void;
};

const DEFAULT_LIFETIME_MS = 4000;

export function useToasts(): ToastsApi {
  const [toasts, setToasts] = useState<Toast[]>([]);
  const nextIdRef = useRef(1);
  const timersRef = useRef(new Map<number, number>());

  const dismiss = useCallback((id: number) => {
    const timer = timersRef.current.get(id);
    if (timer !== undefined) {
      window.clearTimeout(timer);
      timersRef.current.delete(id);
    }
    setToasts((current) => current.filter((toast) => toast.id !== id));
  }, []);

  const show = useCallback(
    (message: string, tone: Toast["tone"] = "info") => {
      const id = nextIdRef.current++;
      setToasts((current) => [...current, { id, message, tone }]);
      const timer = window.setTimeout(() => {
        dismiss(id);
      }, DEFAULT_LIFETIME_MS);
      timersRef.current.set(id, timer);
    },
    [dismiss],
  );

  useEffect(() => {
    return () => {
      for (const timer of timersRef.current.values()) {
        window.clearTimeout(timer);
      }
      timersRef.current.clear();
    };
  }, []);

  return { toasts, show, dismiss };
}

export function ToastViewport({ toasts, dismiss }: { toasts: Toast[]; dismiss: (id: number) => void }) {
  if (toasts.length === 0) return null;
  return (
    <div aria-live="polite" className="toast-viewport">
      {toasts.map((toast) => (
        <button
          className={`toast toast-${toast.tone}`}
          key={toast.id}
          onClick={() => dismiss(toast.id)}
          type="button"
        >
          {toast.message}
        </button>
      ))}
    </div>
  );
}
