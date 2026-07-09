import { useQuery } from "@tanstack/react-query";
import { NavLink, Outlet } from "react-router";
import { api, clearToken } from "../lib/api";

const NAV = [
  { to: "/", label: "Overview" },
  { to: "/builders", label: "Builders" },
  { to: "/demotions", label: "Demotions" },
  { to: "/payloads", label: "Payloads" },
  { to: "/settings", label: "Settings" },
];

export default function Layout() {
  const { data: overview } = useQuery({
    queryKey: ["overview"],
    queryFn: api.overview,
    refetchInterval: 15_000,
  });

  const killSwitchOn = overview?.kill_switch_enabled === true;

  return (
    <div className="min-h-screen bg-neutral-50 text-neutral-900 dark:bg-neutral-950 dark:text-neutral-100">
      {killSwitchOn && (
        <div className="flex items-center justify-center gap-2 bg-status-critical px-4 py-2 text-sm font-medium text-white">
          <span aria-hidden>⚠</span>
          Kill switch is engaged — the relay is not accepting bids.
        </div>
      )}
      <div className="mx-auto flex max-w-7xl">
        <aside className="sticky top-0 flex h-screen w-52 shrink-0 flex-col border-r border-neutral-200 px-4 py-6 dark:border-neutral-800">
          <div className="px-2 text-base font-semibold">Helix Admin</div>
          <nav className="mt-6 flex flex-col gap-1">
            {NAV.map(({ to, label }) => (
              <NavLink
                key={to}
                to={to}
                end={to === "/"}
                className={({ isActive }) =>
                  `rounded-md px-2 py-1.5 text-sm ${
                    isActive
                      ? "bg-neutral-200 font-medium dark:bg-neutral-800"
                      : "text-neutral-600 hover:bg-neutral-100 dark:text-neutral-400 dark:hover:bg-neutral-900"
                  }`
                }
              >
                {label}
              </NavLink>
            ))}
          </nav>
          <button
            onClick={() => {
              clearToken();
              location.reload();
            }}
            className="mt-auto rounded-md px-2 py-1.5 text-left text-sm text-neutral-500 hover:bg-neutral-100 dark:text-neutral-400 dark:hover:bg-neutral-900"
          >
            Sign out
          </button>
        </aside>
        <main className="min-w-0 flex-1 px-8 py-8">
          <Outlet />
        </main>
      </div>
    </div>
  );
}
