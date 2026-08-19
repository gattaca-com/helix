import { useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { type Builder, api, formatEth, shortHex } from "../lib/api";
import { useBuilders } from "../hooks/useBuilders";
import ConfirmDialog from "../components/ConfirmDialog";

export default function PendingPromotion() {
  const queryClient = useQueryClient();
  const [pendingBuilder, setPendingBuilder] = useState<Builder | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);

  const { data, isLoading, error } = useBuilders();

  const mutation = useMutation({
    mutationFn: async (builder: Builder) => {
      await api.promoteBuilder(builder.pub_key);
    },
    onSuccess: () => {
      setPendingBuilder(null);
      setActionError(null);
      queryClient.invalidateQueries({ queryKey: ["builders"] });
      queryClient.invalidateQueries({ queryKey: ["demotions"] });
    },
    onError: (err) => setActionError(err.message),
  });

  if (isLoading) return <p className="text-sm text-neutral-500">Loading…</p>;
  if (error || !data)
    return <p className="text-sm text-status-critical">Failed to load builders.</p>;

  const pending = data.filter((b) => !b.is_optimistic && BigInt(b.collateral) > 0n);

  return (
    <div>
      <p className="text-sm text-neutral-500 dark:text-neutral-400">
        {pending.length} builder{pending.length === 1 ? "" : "s"} are collateralized but not
        optimistic — review and promote them to enable optimistic processing of their bids.
      </p>
      {actionError && (
        <p className="mt-2 text-sm text-status-critical">Action failed: {actionError}</p>
      )}
      {pending.length === 0 ? (
        <p className="mt-6 text-sm text-neutral-500 dark:text-neutral-400">Nothing pending.</p>
      ) : (
        <div className="mt-6 overflow-x-auto rounded-xl border border-neutral-200 dark:border-neutral-800">
          <table className="w-full bg-white text-sm dark:bg-neutral-900">
            <thead>
              <tr className="border-b border-neutral-200 text-left text-neutral-500 dark:border-neutral-800 dark:text-neutral-400">
                <th className="px-4 py-3 font-medium">Builder</th>
                <th className="px-4 py-3 font-medium">Pubkey</th>
                <th className="px-4 py-3 text-right font-medium">Collateral (ETH)</th>
                <th className="px-4 py-3 text-right font-medium">Actions</th>
              </tr>
            </thead>
            <tbody>
              {pending.map((builder) => (
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
                  <td className="px-4 py-3 text-right">
                    <button
                      onClick={() => setPendingBuilder(builder)}
                      className="rounded-md border border-neutral-300 px-2.5 py-1 text-xs font-medium hover:bg-neutral-100 dark:border-neutral-700 dark:hover:bg-neutral-800"
                    >
                      Promote
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {pendingBuilder && (
        <ConfirmDialog
          title="Promote builder to optimistic"
          confirmLabel="Promote"
          pending={mutation.isPending}
          onConfirm={() => mutation.mutate(pendingBuilder)}
          onCancel={() => {
            setPendingBuilder(null);
            setActionError(null);
          }}
        >
          <span className="font-mono text-xs">{pendingBuilder.pub_key}</span>
        </ConfirmDialog>
      )}
    </div>
  );
}
