import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "./styles/tokens.css";
import "./styles/base.css";
import "./styles/app.css";
import "./styles/floating.css";
import { App } from "./App";
import { installNativeMenuGuard } from "./lib/nativeMenu";
import { getState } from "./store/store";

installNativeMenuGuard();
// The saved layout applies before React's first paint, so Floating never flashes Classic.
document.documentElement.dataset.layout = getState().layout;

const root = document.getElementById("root");
if (root) {
  createRoot(root).render(
    <StrictMode>
      <App />
    </StrictMode>,
  );
}
