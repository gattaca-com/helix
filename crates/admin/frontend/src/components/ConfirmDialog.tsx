import { type ReactNode, useState } from "react";

interface Props {
  title: string;
  children?: ReactNode;
  confirmLabel: string;
  /** When set, the user must type this exact string to enable the confirm button. */
  typeToConfirm?: string;
  /** When set, shows a free-text input and passes its value to onConfirm. */
  reasonPlaceholder?: string;
  destructive?: boolean;
  pending?: boolean;
  onConfirm: (reason: string) => void;
  onCancel: () => void;
}

export default function ConfirmDialog({
  title,
  children,
  confirmLabel,
  typeToConfirm,
  reasonPlaceholder,
  destructive = false,
  pending = false,
  onConfirm,
  onCancel,
}: Props) {
  const [typed, setTyped] = useState("");
  const [reason, setReason] = useState("");

  const confirmBlocked = typeToConfirm !== undefined && typed !== typeToConfirm;

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4"
      onClick={onCancel}
    >
      <div
        className="w-full max-w-md rounded-xl border border-neutral-200 bg-white p-6 shadow-lg dark:border-neutral-800 dark:bg-neutral-900"
        onClick={(e) => e.stopPropagation()}
      >
        <h2 className="text-base font-semibold">{title}</h2>
        {children && (
          <div className="mt-2 text-sm text-neutral-600 dark:text-neutral-400">{children}</div>
        )}
        {reasonPlaceholder !== undefined && (
          <input
            value={reason}
            onChange={(e) => setReason(e.target.value)}
            placeholder={reasonPlaceholder}
            autoFocus
            className="mt-4 w-full rounded-md border border-neutral-300 bg-white px-3 py-2 text-sm dark:border-neutral-700 dark:bg-neutral-950"
          />
        )}
        {typeToConfirm !== undefined && (
          <div className="mt-4">
            <p className="text-sm text-neutral-600 dark:text-neutral-400">
              Type <span className="font-mono font-medium">{typeToConfirm}</span> to confirm.
            </p>
            <input
              value={typed}
              onChange={(e) => setTyped(e.target.value)}
              autoFocus={reasonPlaceholder === undefined}
              className="mt-2 w-full rounded-md border border-neutral-300 bg-white px-3 py-2 font-mono text-sm dark:border-neutral-700 dark:bg-neutral-950"
            />
          </div>
        )}
        <div className="mt-6 flex justify-end gap-2">
          <button
            onClick={onCancel}
            className="rounded-md border border-neutral-300 px-3 py-1.5 text-sm hover:bg-neutral-100 dark:border-neutral-700 dark:hover:bg-neutral-800"
          >
            Cancel
          </button>
          <button
            onClick={() => onConfirm(reason)}
            disabled={confirmBlocked || pending}
            className={`rounded-md px-3 py-1.5 text-sm font-medium text-white disabled:opacity-50 ${
              destructive
                ? "bg-status-critical hover:brightness-110"
                : "bg-blue-600 hover:bg-blue-700"
            }`}
          >
            {pending ? "Working…" : confirmLabel}
          </button>
        </div>
      </div>
    </div>
  );
}
