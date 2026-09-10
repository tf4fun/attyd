import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./app";
import { initializeTheme } from "./lib/theme";
import "./styles.css";

const disposeTheme = initializeTheme();
if (import.meta.hot) import.meta.hot.dispose(disposeTheme);

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
