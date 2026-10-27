import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./app";
import { initializeTheme } from "./lib/theme";
import { initializeViewport } from "./lib/viewport";
import "./styles.css";

const disposeTheme = initializeTheme();
const disposeViewport = initializeViewport();
if (import.meta.hot) import.meta.hot.dispose(() => { disposeTheme(); disposeViewport(); });

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
