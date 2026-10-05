import { NavLink, Outlet } from "react-router-dom";
import { logout } from "../api";

const NAV = [
  { to: "/fleet", label: "Fleet" },
  { to: "/settings", label: "Settings" },
  { to: "/plugins", label: "Plugins" },
  { to: "/config-history", label: "Config history" },
  { to: "/tunnel", label: "Tunnel addresses" },
];

export function Layout({ onLoggedOut }: { onLoggedOut: () => void }) {
  return (
    <div className="flex h-screen">
      <aside className="flex w-56 shrink-0 flex-col border-r border-line bg-surface">
        <div className="flex items-center gap-2 px-4 py-4">
          <span className="h-2 w-2 rounded-full bg-accent" />
          <span className="font-mono text-sm font-semibold tracking-tight text-ink">wayhouse fleet</span>
        </div>
        <nav className="flex flex-1 flex-col gap-0.5 px-2">
          {NAV.map((item) => (
            <NavLink
              key={item.to}
              to={item.to}
              className={({ isActive }) =>
                `rounded px-3 py-2 text-sm font-medium transition-colors ${
                  isActive
                    ? "bg-surface-raised text-ink"
                    : "text-ink-muted hover:bg-surface-raised hover:text-ink"
                }`
              }
            >
              {item.label}
            </NavLink>
          ))}
        </nav>
        <button
          onClick={() => logout().finally(onLoggedOut)}
          className="mx-2 mb-4 rounded px-3 py-2 text-left text-sm text-ink-muted hover:bg-surface-raised hover:text-ink"
        >
          Sign out
        </button>
      </aside>
      <main className="flex-1 overflow-y-auto px-8 py-6">
        <Outlet />
      </main>
    </div>
  );
}
