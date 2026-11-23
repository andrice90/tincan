// When the WebView's own right-click menu may open. Its page menu (Back, Refresh, Save
// as, Print) does nothing useful in the app; the text menu (Cut, Copy, Paste) does.

/** What was right-clicked, reduced to the facts the decision needs. */
export interface ContextTarget {
  /** A text input, textarea or contenteditable element. */
  editable: boolean;
  /** The page has a non-empty text selection. */
  hasSelection: boolean;
}

/** Input types that hold text the user can cut, copy or paste. */
const TEXT_INPUT_TYPES = new Set(["", "text", "search", "url", "tel", "email", "password", "number"]);

/** True for `<input type>` values that take typed text (not range, checkbox, file, colour). */
export function isTextInputType(type: string): boolean {
  return TEXT_INPUT_TYPES.has(type.toLowerCase());
}

/** True when the browser's menu should open; false when the page should suppress it. */
export function allowNativeMenu(t: ContextTarget): boolean {
  return t.editable || t.hasSelection;
}

function isEditable(target: EventTarget | null): boolean {
  if (target instanceof HTMLInputElement) return isTextInputType(target.type);
  if (target instanceof HTMLTextAreaElement) return true;
  return target instanceof HTMLElement && target.isContentEditable;
}

/**
 * Suppresses the WebView's page menu everywhere but text fields and selected text. Only
 * calls preventDefault, so the app's own menus (React `onContextMenu`) open as before.
 */
export function installNativeMenuGuard(): void {
  document.addEventListener("contextmenu", (e) => {
    const selection = document.getSelection();
    const hasSelection = selection !== null && !selection.isCollapsed && selection.toString().length > 0;
    if (!allowNativeMenu({ editable: isEditable(e.target), hasSelection })) e.preventDefault();
  });
}
