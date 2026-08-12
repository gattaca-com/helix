import { NavLink, Outlet } from "react-router";

const TABS = [
  { to: "/builders/pending", label: "Pending Promotion" },
  { to: "/builders/groups", label: "Builder Groups" },
];

export default function Builders() {
  return (
    <div>
      <h1 className="text-xl font-semibold">Builders</h1>
      <nav className="mt-4 flex gap-1 border-b border-neutral-200 dark:border-neutral-800">
        {TABS.map(({ to, label }) => (
          <NavLink
            key={to}
            to={to}
            className={({ isActive }) =>
              `-mb-px border-b-2 px-3 py-2 text-sm font-medium ${
                isActive
                  ? "border-neutral-900 text-neutral-900 dark:border-neutral-100 dark:text-neutral-100"
                  : "border-transparent text-neutral-500 hover:text-neutral-700 dark:text-neutral-400 dark:hover:text-neutral-200"
              }`
            }
          >
            {label}
          </NavLink>
        ))}
      </nav>
      <div className="mt-6">
        <Outlet />
      </div>
    </div>
  );
}
