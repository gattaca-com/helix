import { useState } from "react";
import { Link } from "react-router";
import { formatEth, shortHex } from "../lib/api";
import { groupKeyFor, useBuilders } from "../hooks/useBuilders";
import { AddBuilderPubkeyDialog } from "../components/BuilderFormDialog";

export default function BuilderGroups() {
  const { data, isLoading, error } = useBuilders();
  const [showAddBuilder, setShowAddBuilder] = useState(false);

  if (isLoading) return <p className="text-sm text-neutral-500">Loading…</p>;
  if (error || !data)
    return <p className="text-sm text-status-critical">Failed to load builders.</p>;

  const groups = new Map<
    string,
    { builderId: string | null; pubKey: string; count: number; collateral: bigint; optimistic: number }
  >();
  for (const builder of data) {
    const key = groupKeyFor(builder);
    const existing = groups.get(key);
    const collateral = BigInt(builder.collateral);
    if (existing) {
      existing.count += 1;
      existing.collateral += collateral;
      existing.optimistic += builder.is_optimistic ? 1 : 0;
    } else {
      groups.set(key, {
        builderId: builder.builder_id,
        pubKey: builder.pub_key,
        count: 1,
        collateral,
        optimistic: builder.is_optimistic ? 1 : 0,
      });
    }
  }

  const rows = [...groups.entries()].sort(([a], [b]) => a.localeCompare(b));

  return (
    <div>
      <div className="flex items-center justify-between">
        <p className="text-sm text-neutral-500 dark:text-neutral-400">
          {rows.length} builder group{rows.length === 1 ? "" : "s"}.
        </p>
        <button
          onClick={() => setShowAddBuilder(true)}
          className="rounded-md bg-blue-600 px-3 py-1.5 text-xs font-medium text-white hover:bg-blue-700"
        >
          Add builder
        </button>
      </div>
      <div className="mt-6 overflow-x-auto rounded-xl border border-neutral-200 dark:border-neutral-800">
        <table className="w-full bg-white text-sm dark:bg-neutral-900">
          <thead>
            <tr className="border-b border-neutral-200 text-left text-neutral-500 dark:border-neutral-800 dark:text-neutral-400">
              <th className="px-4 py-3 font-medium">Builder</th>
              <th className="px-4 py-3 text-right font-medium">Pubkeys</th>
              <th className="px-4 py-3 text-right font-medium">Total collateral (ETH)</th>
              <th className="px-4 py-3 font-medium">Optimistic</th>
            </tr>
          </thead>
          <tbody>
            {rows.map(([key, group]) => (
              <tr
                key={key}
                className="border-b border-neutral-100 last:border-0 dark:border-neutral-800/50"
              >
                <td className="px-4 py-3">
                  <Link
                    to={`/builders/groups/${key}`}
                    className="font-medium text-blue-600 hover:underline dark:text-blue-400"
                  >
                    {group.builderId ?? (
                      <span className="font-mono text-xs">{shortHex(group.pubKey)}</span>
                    )}
                  </Link>
                </td>
                <td className="px-4 py-3 text-right tabular-nums">{group.count}</td>
                <td className="px-4 py-3 text-right tabular-nums">
                  {formatEth(group.collateral.toString())}
                </td>
                <td className="px-4 py-3">
                  {group.optimistic === group.count ? (
                    <span className="inline-flex items-center gap-1.5 rounded-full bg-status-good/10 px-2 py-0.5 text-xs font-medium text-status-good">
                      ● {group.optimistic}/{group.count}
                    </span>
                  ) : group.optimistic === 0 ? (
                    <span className="inline-flex items-center gap-1.5 rounded-full bg-neutral-500/10 px-2 py-0.5 text-xs font-medium text-neutral-500">
                      ○ {group.optimistic}/{group.count}
                    </span>
                  ) : (
                    <span className="inline-flex items-center gap-1.5 rounded-full bg-status-warning/10 px-2 py-0.5 text-xs font-medium text-status-warning">
                      ◐ {group.optimistic}/{group.count}
                    </span>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      {showAddBuilder && <AddBuilderPubkeyDialog onClose={() => setShowAddBuilder(false)} />}
    </div>
  );
}
