import { useEffect, useState } from "react";
import { checkSession, logout } from "./api";
import { Login } from "./components/Login";
import { FleetView } from "./components/FleetView";
import { ConfigView } from "./components/ConfigView";

type Tab = "fleet" | "config";

export function App() {
  const [authenticated, setAuthenticated] = useState<boolean | null>(null);
  const [tab, setTab] = useState<Tab>("fleet");

  useEffect(() => {
    checkSession().then(setAuthenticated);
  }, []);

  if (authenticated === null) {
    return <p className="loading">loading…</p>;
  }
  if (!authenticated) {
    return <Login onLoggedIn={() => setAuthenticated(true)} />;
  }

  return (
    <div className="app">
      <header>
        <h1>gsp fleet</h1>
        <nav>
          <button className={tab === "fleet" ? "active" : ""} onClick={() => setTab("fleet")}>
            fleet
          </button>
          <button className={tab === "config" ? "active" : ""} onClick={() => setTab("config")}>
            config
          </button>
        </nav>
        <button
          className="logout"
          onClick={() => logout().finally(() => setAuthenticated(false))}
        >
          log out
        </button>
      </header>
      <main>{tab === "fleet" ? <FleetView /> : <ConfigView />}</main>
    </div>
  );
}
