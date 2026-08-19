import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../lib/api";
import ConfirmDialog from "../components/ConfirmDialog";

type Action = "engage-kill-switch" | "disarm-kill-switch" | "disable-adjustments";

export default function Settings() {
  const queryClient = useQueryClient();
  const [pending, setPending] = useState<Action | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);

  const { data: overview } = useQuery({
    queryKey: ["overview"],
    queryFn: api.overview,
    refetchInterval: 15_000,
  });

  const mutation = useMutation({
    mutationFn: async (action: Action) => {
      if (action === "engage-kill-switch") await api.setKillSwitch(true);
      else if (action === "disarm-kill-switch") await api.setKillSwitch(false);
      else await api.disableAdjustments();
    },
    onSuccess: () => {
      setPending(null);
      setActionError(null);
      queryClient.invalidateQueries({ queryKey: ["overview"] });
    },
    onError: (err) => setActionError(err.message),
  });

  const killSwitchOn = overview?.kill_switch_enabled === true;

  return (
    <div>
      <h1 className="text-xl font-semibold">Settings</h1>
      {actionError && (
        <p className="mt-2 text-sm text-status-critical">Action failed: {actionError}</p>
      )}

      <section className="mt-6 rounded-xl border border-status-critical/40 bg-white p-6 dark:bg-neutral-900">
        <h2 className="flex items-center gap-2 text-base font-semibold text-status-critical">
          <span aria-hidden>⚠</span> Danger zone
        </h2>

        <div className="mt-4 flex items-center justify-between gap-4 border-b border-neutral-100 pb-4 dark:border-neutral-800">
          <div>
            <div className="font-medium">Kill switch</div>
            <p className="text-sm text-neutral-500 dark:text-neutral-400">
              {overview?.kill_switch_enabled === null
                ? "Relay admin API unreachable — state unknown."
                : killSwitchOn
                  ? "Engaged. The relay is refusing all bids."
                  : "Disarmed. The relay is operating normally."}
            </p>
          </div>
          {killSwitchOn ? (
            <button
              onClick={() => setPending("disarm-kill-switch")}
              className="shrink-0 rounded-md border border-neutral-300 px-3 py-1.5 text-sm font-medium hover:bg-neutral-100 dark:border-neutral-700 dark:hover:bg-neutral-800"
            >
              Disarm kill switch
            </button>
          ) : (
            <button
              onClick={() => setPending("engage-kill-switch")}
              className="shrink-0 rounded-md bg-status-critical px-3 py-1.5 text-sm font-medium text-white hover:brightness-110"
            >
              Engage kill switch
            </button>
          )}
        </div>

        <div className="mt-4 flex items-center justify-between gap-4">
          <div>
            <div className="font-medium">Bid adjustments</div>
            <p className="text-sm text-neutral-500 dark:text-neutral-400">
              {overview?.adjustments_enabled
                ? "Enabled. Disabling is one-way from this UI."
                : "Disabled."}
            </p>
          </div>
          <button
            onClick={() => setPending("disable-adjustments")}
            disabled={overview?.adjustments_enabled === false}
            className="shrink-0 rounded-md border border-status-critical/40 px-3 py-1.5 text-sm font-medium text-status-critical hover:bg-status-critical/10 disabled:opacity-50"
          >
            Disable adjustments
          </button>
        </div>
      </section>

      {pending === "engage-kill-switch" && (
        <ConfirmDialog
          title="Engage kill switch"
          confirmLabel="Engage"
          destructive
          typeToConfirm="engage"
          pending={mutation.isPending}
          onConfirm={() => mutation.mutate(pending)}
          onCancel={() => setPending(null)}
        >
          The relay will immediately stop accepting bids. This affects live block production.
        </ConfirmDialog>
      )}
      {pending === "disarm-kill-switch" && (
        <ConfirmDialog
          title="Disarm kill switch"
          confirmLabel="Disarm"
          pending={mutation.isPending}
          onConfirm={() => mutation.mutate(pending)}
          onCancel={() => setPending(null)}
        >
          The relay will resume accepting bids.
        </ConfirmDialog>
      )}
      {pending === "disable-adjustments" && (
        <ConfirmDialog
          title="Disable bid adjustments"
          confirmLabel="Disable"
          destructive
          typeToConfirm="disable"
          pending={mutation.isPending}
          onConfirm={() => mutation.mutate(pending)}
          onCancel={() => setPending(null)}
        >
          Bid adjustments will be turned off for all builders. Re-enabling requires direct
          database access.
        </ConfirmDialog>
      )}
    </div>
  );
}
