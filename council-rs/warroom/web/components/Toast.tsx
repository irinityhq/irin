"use client";

import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { AnimatePresence, motion } from "framer-motion";
import { AlertTriangle, CheckCircle, Info, X } from "lucide-react";
import { cn } from "@/lib/cn";

export type ToastType = "error" | "success" | "info";

export interface ToastItem {
  id: string;
  type: ToastType;
  message: string;
}

interface ToastContextValue {
  toast: (type: ToastType, message: string) => void;
}

const ToastContext = createContext<ToastContextValue | null>(null);

const MAX_VISIBLE = 3;
const DURATION_MS: Record<ToastType, number> = {
  error: 8000,
  success: 5000,
  info: 5000,
};

export function ToastProvider({ children }: { children: React.ReactNode }) {
  const [toasts, setToasts] = useState<ToastItem[]>([]);
  const timersRef = useRef<Map<string, ReturnType<typeof setTimeout>>>(new Map());

  const dismiss = useCallback((id: string) => {
    setToasts((prev) => prev.filter((t) => t.id !== id));
    const timer = timersRef.current.get(id);
    if (timer) {
      clearTimeout(timer);
      timersRef.current.delete(id);
    }
  }, []);

  const toast = useCallback(
    (type: ToastType, message: string) => {
      const id =
        typeof crypto !== "undefined" && "randomUUID" in crypto
          ? crypto.randomUUID()
          : Math.random().toString(36).slice(2, 10) + Date.now().toString(36);
      setToasts((prev) => {
        const next = [...prev, { id, type, message }];
        // Cap at MAX_VISIBLE; drop the oldest.
        if (next.length > MAX_VISIBLE) {
          const dropped = next.slice(0, next.length - MAX_VISIBLE);
          for (const d of dropped) {
            const t = timersRef.current.get(d.id);
            if (t) {
              clearTimeout(t);
              timersRef.current.delete(d.id);
            }
          }
          return next.slice(-MAX_VISIBLE);
        }
        return next;
      });
      const timer = setTimeout(() => dismiss(id), DURATION_MS[type]);
      timersRef.current.set(id, timer);
    },
    [dismiss],
  );

  useEffect(() => {
    const timers = timersRef.current;
    return () => {
      timers.forEach((t) => clearTimeout(t));
      timers.clear();
    };
  }, []);

  const value = useMemo(() => ({ toast }), [toast]);

  return (
    <ToastContext.Provider value={value}>
      {children}
      <ToastViewport toasts={toasts} onDismiss={dismiss} />
    </ToastContext.Provider>
  );
}

export function useToast(): ToastContextValue {
  const ctx = useContext(ToastContext);
  if (!ctx) {
    throw new Error("useToast must be used within a ToastProvider");
  }
  return ctx;
}

function ToastViewport({
  toasts,
  onDismiss,
}: {
  toasts: ToastItem[];
  onDismiss: (id: string) => void;
}) {
  return (
    <div
      aria-live="polite"
      aria-atomic="false"
      className="pointer-events-none fixed bottom-4 right-4 z-50 flex w-[min(92vw,360px)] flex-col gap-2"
    >
      <AnimatePresence initial={false}>
        {toasts.map((t) => (
          <ToastCard key={t.id} item={t} onDismiss={() => onDismiss(t.id)} />
        ))}
      </AnimatePresence>
    </div>
  );
}

function ToastCard({
  item,
  onDismiss,
}: {
  item: ToastItem;
  onDismiss: () => void;
}) {
  const Icon =
    item.type === "error"
      ? AlertTriangle
      : item.type === "success"
        ? CheckCircle
        : Info;

  return (
    <motion.div
      layout
      initial={{ opacity: 0, x: 24, scale: 0.96 }}
      animate={{ opacity: 1, x: 0, scale: 1 }}
      exit={{ opacity: 0, x: 24, scale: 0.96 }}
      transition={{ duration: 0.18, ease: "easeOut" }}
      role={item.type === "error" ? "alert" : "status"}
      className={cn(
        "pointer-events-auto flex items-start gap-2.5 rounded-md border px-3 py-2.5 shadow-lg backdrop-blur",
        "bg-bg-overlay/90",
        item.type === "error" && "border-danger/40 bg-danger/10",
        item.type === "success" && "border-success/40 bg-success/10",
        item.type === "info" && "border-cyan/40 bg-cyan/10",
      )}
    >
      <Icon
        className={cn(
          "mt-0.5 h-4 w-4 shrink-0",
          item.type === "error" && "text-danger",
          item.type === "success" && "text-success",
          item.type === "info" && "text-cyan",
        )}
      />
      <div
        className={cn(
          "flex-1 text-xs leading-snug font-mono",
          item.type === "error" && "text-danger",
          item.type === "success" && "text-success",
          item.type === "info" && "text-cyan",
        )}
      >
        {item.message}
      </div>
      <button
        type="button"
        onClick={onDismiss}
        className="text-fg-dim hover:text-fg shrink-0 rounded p-0.5 transition-colors"
        aria-label="Dismiss notification"
      >
        <X className="h-3.5 w-3.5" />
      </button>
    </motion.div>
  );
}
