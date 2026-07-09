import { useQuery } from "@tanstack/react-query";
import { api, shortHex } from "../lib/api";

export default function Demotions() {
  const { data, isLoading, error } = useQuery({
    queryKey: ["demotions"],
    queryFn: () => api.demotions(200),
  });

  if (isLoading) return <p className="text-sm text-neutral-500">Loading…</p>;
  if (error || !data)
    return <p className="text-sm text-status-critical">Failed to load demotions.</p>;

  return (
    <div>
      <h1 className="text-xl font-semibold">Demotions</h1>
      <p className="mt-1 text-sm text-neutral-500 dark:text-neutral-400">
        Most recent {data.length} builder demotions. Slot 0 marks a manual demotion via the
        admin API.
      </p>
      <div className="mt-6 overflow-x-auto rounded-xl border border-neutral-200 dark:border-neutral-800">
        <table className="w-full bg-white text-sm dark:bg-neutral-900">
          <thead>
            <tr className="border-b border-neutral-200 text-left text-neutral-500 dark:border-neutral-800 dark:text-neutral-400">
              <th className="px-4 py-3 font-medium">Time</th>
              <th className="px-4 py-3 font-medium">Builder pubkey</th>
              <th className="px-4 py-3 text-right font-medium">Slot</th>
              <th className="px-4 py-3 font-medium">Block hash</th>
              <th className="px-4 py-3 font-medium">Reason</th>
            </tr>
          </thead>
          <tbody>
            {data.length === 0 && (
              <tr>
                <td colSpan={5} className="px-4 py-6 text-center text-neutral-500">
                  No demotions recorded.
                </td>
              </tr>
            )}
            {data.map((demotion, i) => (
              <tr
                key={`${demotion.public_key}-${demotion.demotion_time_ms}-${i}`}
                className="border-b border-neutral-100 last:border-0 dark:border-neutral-800/50"
              >
                <td className="px-4 py-3 whitespace-nowrap tabular-nums">
                  {new Date(demotion.demotion_time_ms).toLocaleString()}
                </td>
                <td className="px-4 py-3 font-mono text-xs" title={demotion.public_key}>
                  {shortHex(demotion.public_key)}
                </td>
                <td className="px-4 py-3 text-right tabular-nums">
                  {demotion.slot_number ?? "—"}
                </td>
                <td className="px-4 py-3 font-mono text-xs" title={demotion.block_hash ?? ""}>
                  {demotion.block_hash ? shortHex(demotion.block_hash) : "—"}
                </td>
                <td className="max-w-md px-4 py-3">
                  <span className="line-clamp-2">{demotion.reason ?? "—"}</span>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}
