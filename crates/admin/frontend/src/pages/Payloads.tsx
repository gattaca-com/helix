import { useState } from "react";
import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { api, type DeliveredPayload, formatEth, shortHex } from "../lib/api";

const PAGE_SIZE = 50;

export default function Payloads() {
  const [pages, setPages] = useState<DeliveredPayload[][]>([]);
  const [cursor, setCursor] = useState<string | undefined>(undefined);

  const { data, isLoading, isFetching, error } = useQuery({
    queryKey: ["payloads", cursor],
    queryFn: () => api.payloads({ cursor, limit: PAGE_SIZE }),
    placeholderData: keepPreviousData,
  });

  const seen = new Set<string>();
  const rows = [...pages.flat(), ...(data ?? [])].filter((p) => {
    if (seen.has(p.block_hash)) return false;
    seen.add(p.block_hash);
    return true;
  });

  function loadMore() {
    if (!data || data.length === 0) return;
    const lastSlot = data[data.length - 1].slot;
    setPages((prev) => [...prev, data]);
    setCursor(String(BigInt(lastSlot) - 1n));
  }

  if (isLoading && rows.length === 0)
    return <p className="text-sm text-neutral-500">Loading…</p>;
  if (error && rows.length === 0)
    return <p className="text-sm text-status-critical">Failed to load payloads.</p>;

  const lastPageExhausted = (data?.length ?? 0) < PAGE_SIZE;

  return (
    <div>
      <h1 className="text-xl font-semibold">Delivered payloads</h1>
      <p className="mt-1 text-sm text-neutral-500 dark:text-neutral-400">
        Most recent payloads delivered to proposers, newest first.
      </p>
      <div className="mt-6 overflow-x-auto rounded-xl border border-neutral-200 dark:border-neutral-800">
        <table className="w-full bg-white text-sm dark:bg-neutral-900">
          <thead>
            <tr className="border-b border-neutral-200 text-left text-neutral-500 dark:border-neutral-800 dark:text-neutral-400">
              <th className="px-4 py-3 text-right font-medium">Slot</th>
              <th className="px-4 py-3 text-right font-medium">Block</th>
              <th className="px-4 py-3 font-medium">Block hash</th>
              <th className="px-4 py-3 font-medium">Builder</th>
              <th className="px-4 py-3 text-right font-medium">Value (ETH)</th>
              <th className="px-4 py-3 text-right font-medium">Txs</th>
              <th className="px-4 py-3 text-right font-medium">Gas used</th>
            </tr>
          </thead>
          <tbody>
            {rows.length === 0 && (
              <tr>
                <td colSpan={7} className="px-4 py-6 text-center text-neutral-500">
                  No delivered payloads.
                </td>
              </tr>
            )}
            {rows.map((payload) => (
              <tr
                key={payload.block_hash}
                className="border-b border-neutral-100 last:border-0 dark:border-neutral-800/50"
              >
                <td className="px-4 py-3 text-right tabular-nums">{payload.slot}</td>
                <td className="px-4 py-3 text-right tabular-nums">{payload.block_number}</td>
                <td className="px-4 py-3 font-mono text-xs" title={payload.block_hash}>
                  {shortHex(payload.block_hash)}
                </td>
                <td className="px-4 py-3 font-mono text-xs" title={payload.builder_pubkey}>
                  {shortHex(payload.builder_pubkey)}
                </td>
                <td className="px-4 py-3 text-right tabular-nums">{formatEth(payload.value)}</td>
                <td className="px-4 py-3 text-right tabular-nums">{payload.num_tx}</td>
                <td className="px-4 py-3 text-right tabular-nums">
                  {Number(payload.gas_used).toLocaleString("en-US")}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {!lastPageExhausted && (
        <button
          onClick={loadMore}
          disabled={isFetching}
          className="mt-4 rounded-md border border-neutral-300 px-3 py-1.5 text-sm hover:bg-neutral-100 disabled:opacity-50 dark:border-neutral-700 dark:hover:bg-neutral-800"
        >
          {isFetching ? "Loading…" : "Load more"}
        </button>
      )}
    </div>
  );
}
