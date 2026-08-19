import { useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { api, formatEth, parseEth } from "../lib/api";
import ConfirmDialog from "./ConfirmDialog";

const inputClass =
  "mt-1 w-full rounded-md border border-neutral-300 bg-white px-3 py-2 text-sm dark:border-neutral-700 dark:bg-neutral-950";

const labelClass = "block text-xs font-medium text-neutral-500 dark:text-neutral-400";

const MIN_OPTIMISTIC_COLLATERAL_WEI = 10n ** 18n;

function meetsOptimisticThreshold(collateralEth: string): boolean {
  try {
    return BigInt(parseEth(collateralEth)) > MIN_OPTIMISTIC_COLLATERAL_WEI;
  } catch {
    return false;
  }
}

interface AddBuilderPubkeyDialogProps {
  /** When set, the pubkey is added to this existing builder group (builder_id locked). */
  lockedBuilderId?: string;
  onClose: () => void;
}

export function AddBuilderPubkeyDialog({ lockedBuilderId, onClose }: AddBuilderPubkeyDialogProps) {
  const queryClient = useQueryClient();
  const [pubKey, setPubKey] = useState("");
  const [builderId, setBuilderId] = useState("");
  const [collateral, setCollateral] = useState("0");
  const [isOptimistic, setIsOptimistic] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);

  const eligibleForOptimistic = meetsOptimisticThreshold(collateral);

  const mutation = useMutation({
    mutationFn: async () => {
      let collateralWei: string;
      try {
        collateralWei = parseEth(collateral);
      } catch (err) {
        throw err instanceof Error ? err : new Error("invalid collateral");
      }
      await api.createBuilder({
        pub_key: pubKey.trim(),
        builder_id: lockedBuilderId ?? (builderId.trim() || null),
        collateral: collateralWei,
        is_optimistic: isOptimistic && eligibleForOptimistic,
      });
    },
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["builders"] });
      onClose();
    },
    onError: (err) => setFormError(err.message),
  });

  return (
    <ConfirmDialog
      title={lockedBuilderId ? "Add pubkey to builder" : "Add new builder"}
      confirmLabel="Add"
      pending={mutation.isPending}
      onConfirm={() => {
        setFormError(null);
        mutation.mutate();
      }}
      onCancel={onClose}
    >
      <div className="space-y-3">
        {formError && <p className="text-status-critical">{formError}</p>}
        <div>
          <label className={labelClass}>Pubkey</label>
          <input
            value={pubKey}
            onChange={(e) => setPubKey(e.target.value)}
            placeholder="0x..."
            autoFocus
            className={`${inputClass} font-mono`}
          />
        </div>
        <div>
          <label className={labelClass}>Builder ID</label>
          {lockedBuilderId ? (
            <input value={lockedBuilderId} disabled className={`${inputClass} opacity-60`} />
          ) : (
            <input
              value={builderId}
              onChange={(e) => setBuilderId(e.target.value)}
              placeholder="(optional)"
              className={inputClass}
            />
          )}
        </div>
        <div>
          <label className={labelClass}>Collateral (ETH)</label>
          <input
            value={collateral}
            onChange={(e) => setCollateral(e.target.value)}
            placeholder="0"
            className={`${inputClass} tabular-nums`}
          />
        </div>
        <div>
          <label className="flex items-center gap-2 text-sm">
            <input
              type="checkbox"
              checked={isOptimistic && eligibleForOptimistic}
              disabled={!eligibleForOptimistic}
              onChange={(e) => setIsOptimistic(e.target.checked)}
            />
            Optimistic
          </label>
          {!eligibleForOptimistic && (
            <p className="mt-1 text-xs text-neutral-500 dark:text-neutral-400">
              Requires more than 1 ETH collateral.
            </p>
          )}
        </div>
      </div>
    </ConfirmDialog>
  );
}

interface EditCollateralDialogProps {
  pubKey: string;
  currentCollateral: string;
  onClose: () => void;
}

export function EditCollateralDialog({
  pubKey,
  currentCollateral,
  onClose,
}: EditCollateralDialogProps) {
  const queryClient = useQueryClient();
  const [collateral, setCollateral] = useState(formatEth(currentCollateral));
  const [formError, setFormError] = useState<string | null>(null);

  const mutation = useMutation({
    mutationFn: async () => {
      let collateralWei: string;
      try {
        collateralWei = parseEth(collateral);
      } catch (err) {
        throw err instanceof Error ? err : new Error("invalid collateral");
      }
      await api.updateBuilderCollateral(pubKey, collateralWei);
    },
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["builders"] });
      onClose();
    },
    onError: (err) => setFormError(err.message),
  });

  return (
    <ConfirmDialog
      title="Edit collateral"
      confirmLabel="Save"
      pending={mutation.isPending}
      onConfirm={() => {
        setFormError(null);
        mutation.mutate();
      }}
      onCancel={onClose}
    >
      <div className="space-y-3">
        {formError && <p className="text-status-critical">{formError}</p>}
        <p className="font-mono text-xs">{pubKey}</p>
        <div>
          <label className={labelClass}>Collateral (ETH)</label>
          <input
            value={collateral}
            onChange={(e) => setCollateral(e.target.value)}
            autoFocus
            className={`${inputClass} tabular-nums`}
          />
        </div>
      </div>
    </ConfirmDialog>
  );
}
