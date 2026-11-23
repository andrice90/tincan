// Shared busy/error handling for dialogs whose action can fail.

import { useState } from "react";
import { errorText } from "../api/client";

/** Wraps an async action: tracks busy state and keeps the error text to show inline. */
export function useSubmit(action: () => Promise<void>, onDone?: () => void) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const submit = async () => {
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      await action();
      onDone?.();
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  };
  return { busy, error, setError, submit: () => void submit() };
}
