import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, type Builder, formatEth, shortHex } from "../lib/api";
import ConfirmDialog from "../components/ConfirmDialog";

type PendingAction = { kind: "demote" | "promote"; builder: Builder };

export default function Builders() {
  const queryClient = useQueryClient();
  const [pendingAction, setPendingAction] = useState<PendingAction | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);

  const { data, isLoading, error } = useQuery({ queryKey: ["builders"], queryFn: api.builders });

  const mutation = useMutation({
    mutationFn: async ({ kind, builder, reason }: PendingAction & { reason: string }) => {
      if (kind === "demote") {
        await api.demoteBuilder(builder.pub_key, reason);
      } else {
        await api.promoteBuilder(builder.pub_key);
      }
    },
    onSuccess: () => {
      setPendingAction(null);
      setActionError(null);
      queryClient.invalidateQueries({ queryKey: ["builders"] });
      queryClient.invalidateQueries({ queryKey: ["demotions"] });
    },
    onError: (err) => setActionError(err.message),
  });

  if (isLoading) return <p className="text-sm text-neutral-500">Loading…</p>;
  if (error || !data)
    return <p className="text-sm text-status-critical">Failed to load builders.</p>;

  return (
    <div>
      <h1 className="text-xl font-semibold">Builders</h1>
      <p className="mt-1 text-sm text-neutral-500 dark:text-neutral-400">
        {data.length} builders known to the relay. Demoting a builder disables optimistic
        processing of its bids.
      </p>
      {actionError && (
        <p className="mt-2 text-sm text-status-critical">Action failed: {actionError}</p>
      )}
      <div className="mt-6 overflow-x-auto rounded-xl border border-neutral-200 dark:border-neutral-800">
        <table className="w-full bg-white text-sm dark:bg-neutral-900">
          <thead>
            <tr className="border-b border-neutral-200 text-left text-neutral-500 dark:border-neutral-800 dark:text-neutral-400">
              <th className="px-4 py-3 font-medium">Builder</th>
              <th className="px-4 py-3 font-medium">Pubkey</th>
              <th className="px-4 py-3 text-right font-medium">Collateral (ETH)</th>
              <th className="px-4 py-3 font-medium">Optimistic</th>
              <th className="px-4 py-3 text-right font-medium">Actions</th>
            </tr>
          </thead>
          <tbody>
            {data.map((builder) => (
              <tr
                key={builder.pub_key}
                className="border-b border-neutral-100 last:border-0 dark:border-neutral-800/50"
              >
                <td className="px-4 py-3">{builder.builder_id ?? "—"}</td>
                <td className="px-4 py-3 font-mono text-xs" title={builder.pub_key}>
                  {shortHex(builder.pub_key)}
                </td>
                <td className="px-4 py-3 text-right tabular-nums">
                  {formatEth(builder.collateral)}
                </td>
                <td className="px-4 py-3">
                  {builder.is_optimistic ? (
                    <span className="inline-flex items-center gap-1.5 rounded-full bg-status-good/10 px-2 py-0.5 text-xs font-medium text-status-good">
                      ● Optimistic
                    </span>
                  ) : (
                    <span className="inline-flex items-center gap-1.5 rounded-full bg-neutral-500/10 px-2 py-0.5 text-xs font-medium text-neutral-500">
                      ○ Pessimistic
                    </span>
                  )}
                </td>
                <td className="px-4 py-3 text-right">
                  {builder.is_optimistic ? (
                    <button
                      onClick={() => setPendingAction({ kind: "demote", builder })}
                      className="rounded-md border border-status-critical/40 px-2.5 py-1 text-xs font-medium text-status-critical hover:bg-status-critical/10"
                    >
                      Demote
                    </button>
                  ) : (
                    <button
                      onClick={() => setPendingAction({ kind: "promote", builder })}
                      className="rounded-md border border-neutral-300 px-2.5 py-1 text-xs font-medium hover:bg-neutral-100 dark:border-neutral-700 dark:hover:bg-neutral-800"
                    >
                      Promote
                    </button>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      {pendingAction && (
        <ConfirmDialog
          title={
            pendingAction.kind === "demote"
              ? "Demote builder to pessimistic"
              : "Promote builder to optimistic"
          }
          confirmLabel={pendingAction.kind === "demote" ? "Demote" : "Promote"}
          destructive={pendingAction.kind === "demote"}
          reasonPlaceholder={
            pendingAction.kind === "demote" ? "Reason (optional)" : undefined
          }
          pending={mutation.isPending}
          onConfirm={(reason) => mutation.mutate({ ...pendingAction, reason })}
          onCancel={() => {
            setPendingAction(null);
            setActionError(null);
          }}
        >
          <span className="font-mono text-xs">{pendingAction.builder.pub_key}</span>
        </ConfirmDialog>
      )}
    </div>
  );
}
