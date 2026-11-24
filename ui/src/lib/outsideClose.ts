// When a floating panel that is not a Popover (the soundboard) closes on a click elsewhere.
// It cannot use Popover's rules: those also close on window blur, and the soundboard's
// "Add sound" file picker blurs the window.

/** What the decision needs from the pointerdown target (an Element in the app). */
export interface TargetLike {
  closest(selector: string): unknown;
}

/** What the decision needs from the panel (an Element in the app). */
export interface PanelLike<T> {
  contains(node: T): boolean;
}

/**
 * Elements that keep the panel open although they are outside its DOM: menus and popovers
 * (portaled to the body, such as a tile's right-click menu), dialogs (Add sound, Rename) and
 * the panel's own toggle button, whose click toggles it instead of closing and reopening it.
 */
export const KEEP_OPEN_SELECTOR = ".popover, dialog, .soundboard-toggle";

/** Does a pointerdown on `target` close `panel`? */
export function closesOnPointerDown<T extends TargetLike>(target: T | null, panel: PanelLike<T> | null): boolean {
  if (!target || !panel) return false;
  if (panel.contains(target)) return false;
  return target.closest(KEEP_OPEN_SELECTOR) === null;
}

/** Does Escape close the panel? Not while a menu, popover or dialog has it (they close first). */
export function closesOnEscape(e: { defaultPrevented: boolean }, dialogOpen: boolean): boolean {
  return !e.defaultPrevented && !dialogOpen;
}
