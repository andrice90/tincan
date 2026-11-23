// First launch: the only setup step.

import { useState } from "react";
import { setName } from "../store/actions";
import { useSubmit } from "../dialogs/useSubmit";

export function Welcome() {
  const [name, setValue] = useState("");
  const s = useSubmit(() => setName(name.trim()));
  return (
    <main className="center-screen">
      <form
        className="center-card"
        onSubmit={(e) => {
          e.preventDefault();
          if (name.trim()) s.submit();
        }}
      >
        <h1>Pick a display name</h1>
        <div className="field">
          <label className="field-label" htmlFor="welcome-name">
            Display name
          </label>
          <input
            id="welcome-name"
            className="input"
            value={name}
            maxLength={32}
            autoFocus
            autoComplete="off"
            onChange={(e) => {
              setValue(e.target.value);
              s.setError(null);
            }}
          />
        </div>
        {s.error && (
          <p className="error-text" role="alert">
            {s.error}
          </p>
        )}
        <button type="submit" className="btn btn-primary btn-block" disabled={s.busy || !name.trim()}>
          Continue
        </button>
      </form>
    </main>
  );
}
