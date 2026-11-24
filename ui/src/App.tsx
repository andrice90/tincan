// Root: title bar, the current screen, dialogs and toasts.

import { useEffect, useLayoutEffect } from "react";
import { TitleBar } from "./components/TitleBar";
import { Toasts } from "./components/Toasts";
import { Dialogs } from "./dialogs/Dialogs";
import { Shell } from "./screens/Shell";
import { Welcome } from "./screens/Welcome";
import { init } from "./store/actions";
import { useStore } from "./store/store";

function useTheme(): void {
  const theme = useStore((s) => s.settings?.theme ?? "system");
  useEffect(() => {
    const root = document.documentElement;
    if (theme === "system") delete root.dataset.theme;
    else root.dataset.theme = theme;
  }, [theme]);
}

/** Classic or Floating on <html> (main.tsx sets it before the first paint too). */
function useLayoutAttr(): void {
  const layout = useStore((s) => s.layout);
  useLayoutEffect(() => {
    document.documentElement.dataset.layout = layout;
  }, [layout]);
}

export function App() {
  useEffect(() => init(), []);
  useTheme();
  useLayoutAttr();
  const ready = useStore((s) => s.ready);
  const fatal = useStore((s) => s.fatal);
  const hasMe = useStore((s) => s.me !== null);

  let screen;
  if (!ready) screen = <div className="center-screen" />;
  else if (fatal)
    screen = (
      <main className="center-screen">
        <div className="center-card">
          <h1>Tincan couldn't start</h1>
          <p className="muted">{fatal}</p>
          <button type="button" className="btn btn-primary" onClick={() => location.reload()}>
            Try again
          </button>
        </div>
      </main>
    );
  else if (!hasMe) screen = <Welcome />;
  else screen = <Shell />;

  return (
    <div className="app">
      <TitleBar />
      {screen}
      {ready && !fatal && hasMe && <Dialogs />}
      <Toasts />
    </div>
  );
}
