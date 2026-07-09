import { useQuery } from "@tanstack/react-query";
import { api, formatCount } from "../lib/api";

function StatTile({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded-xl border border-neutral-200 bg-white p-5 dark:border-neutral-800 dark:bg-neutral-900">
      <div className="text-sm text-neutral-500 dark:text-neutral-400">{label}</div>
      <div className="mt-1 text-3xl font-semibold">{value}</div>
    </div>
  );
}

function StatusTile({
  label,
  ok,
  okText,
  badText,
  unknown,
}: {
  label: string;
  ok: boolean;
  okText: string;
  badText: string;
  unknown?: boolean;
}) {
  return (
    <div className="rounded-xl border border-neutral-200 bg-white p-5 dark:border-neutral-800 dark:bg-neutral-900">
      <div className="text-sm text-neutral-500 dark:text-neutral-400">{label}</div>
      {unknown ? (
        <div className="mt-1 flex items-center gap-2 text-lg font-medium text-neutral-500">
          <span aria-hidden>?</span> Unknown — relay unreachable
        </div>
      ) : (
        <div
          className={`mt-1 flex items-center gap-2 text-lg font-medium ${
            ok ? "text-status-good" : "text-status-critical"
          }`}
        >
          <span aria-hidden>{ok ? "●" : "⚠"}</span>
          {ok ? okText : badText}
        </div>
      )}
    </div>
  );
}

export default function Overview() {
  const { data, isLoading, error } = useQuery({
    queryKey: ["overview"],
    queryFn: api.overview,
    refetchInterval: 15_000,
  });

  if (isLoading) return <p className="text-sm text-neutral-500">Loading…</p>;
  if (error || !data)
    return <p className="text-sm text-status-critical">Failed to load overview.</p>;

  return (
    <div>
      <h1 className="text-xl font-semibold">Overview</h1>
      <div className="mt-6 grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3">
        <StatTile label="Network validators" value={formatCount(data.num_network_validators)} />
        <StatTile
          label="Registered validators"
          value={formatCount(data.num_registered_validators)}
        />
        <StatTile label="Delivered payloads" value={formatCount(data.num_delivered_payloads)} />
        <StatusTile
          label="Kill switch"
          ok={data.kill_switch_enabled !== true}
          okText="Disarmed — relay accepting bids"
          badText="Engaged — relay halted"
          unknown={data.kill_switch_enabled === null}
        />
        <StatusTile
          label="Bid adjustments"
          ok={data.adjustments_enabled}
          okText="Enabled"
          badText="Disabled"
        />
      </div>
    </div>
  );
}
