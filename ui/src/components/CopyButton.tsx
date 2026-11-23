// A button that copies text and reads "Copied" for two seconds.

import { useEffect, useState } from "react";
import { copyText, errorText } from "../api/client";

interface CopyButtonProps {
  /** The text, or a function that fetches it on click (its rejection goes to onError). */
  text: string | null | (() => Promise<string>);
  label: string;
  className?: string;
  onError?: (text: string) => void;
}

export function CopyButton({ text, label, className = "btn", onError }: CopyButtonProps) {
  const [copied, setCopied] = useState(false);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!copied) return;
    const t = setTimeout(() => setCopied(false), 2000);
    return () => clearTimeout(t);
  }, [copied]);

  const copy = async () => {
    if (text === null) return;
    setBusy(true);
    try {
      const value = typeof text === "function" ? await text() : text;
      setCopied(await copyText(value));
    } catch (err) {
      onError?.(errorText(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <button type="button" className={className} disabled={text === null || busy} onClick={() => void copy()}>
      {copied ? "Copied" : label}
    </button>
  );
}
