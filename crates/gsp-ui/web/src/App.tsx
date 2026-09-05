import { useEffect, useState } from "react";
import { Navigate, Route, Routes } from "react-router-dom";
import { checkSession } from "./api";
import { Login } from "./components/Login";
import { Layout } from "./components/Layout";
import { FleetPage } from "./pages/FleetPage";
import { SettingsPage } from "./pages/SettingsPage";
import { PluginsPage } from "./pages/PluginsPage";
import { ConfigHistoryPage } from "./pages/ConfigHistoryPage";

export function App() {
  const [authenticated, setAuthenticated] = useState<boolean | null>(null);

  useEffect(() => {
    checkSession().then(setAuthenticated);
  }, []);

  if (authenticated === null) {
    return (
      <div className="flex h-screen items-center justify-center text-sm text-ink-muted">Loading…</div>
    );
  }
  if (!authenticated) {
    return <Login onLoggedIn={() => setAuthenticated(true)} />;
  }

  return (
    <Routes>
      <Route element={<Layout onLoggedOut={() => setAuthenticated(false)} />}>
        <Route index element={<Navigate to="/fleet" replace />} />
        <Route path="/fleet" element={<FleetPage />} />
        <Route path="/settings" element={<SettingsPage />} />
        <Route path="/plugins" element={<PluginsPage />} />
        <Route path="/config-history" element={<ConfigHistoryPage />} />
        <Route path="*" element={<Navigate to="/fleet" replace />} />
      </Route>
    </Routes>
  );
}
