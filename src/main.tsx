import React from "react";
import ReactDOM from "react-dom/client";
import "@fontsource-variable/inter";
import "@fontsource/jetbrains-mono/400.css";
import "@fontsource/jetbrains-mono/500.css";
import "@fontsource/jetbrains-mono/600.css";
import "./index.css";
import App from "./App";
import { ErrorBoundary } from "./components/ErrorBoundary";

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    {/* views have their own boundaries; this one keeps a crash in the nav
        rail, toasts, or the update card from blanking the whole window */}
    <ErrorBoundary label="Trawler">
      <App />
    </ErrorBoundary>
  </React.StrictMode>,
);
