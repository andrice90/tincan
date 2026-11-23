// Popovers and menus: positioned in a portal, kept inside the window, closed
// on Escape, outside clicks, window blur and resize.

import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
  type MouseEvent as ReactMouseEvent,
  type ReactNode,
} from "react";
import { createPortal } from "react-dom";

/** One menu entry, or a separator. */
export type MenuItem =
  | { label: string; icon?: ReactNode; onSelect: () => void; danger?: boolean; disabled?: boolean }
  | "separator";

/** Where to open: a point (context menu) or next to an element. */
export type MenuAnchor = { x: number; y: number } | { rect: DOMRect; align?: "start" | "end"; above?: boolean };

interface PopoverProps {
  anchor: MenuAnchor;
  onClose: () => void;
  label: string;
  role?: "menu" | "dialog";
  className?: string;
  children: ReactNode;
  onKeyDown?: (e: ReactKeyboardEvent<HTMLDivElement>) => void;
}

/** A floating panel anchored to a point or an element. */
export function Popover({ anchor, onClose, label, role = "dialog", className, children, onKeyDown }: PopoverProps) {
  const ref = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState<{ left: number; top: number } | null>(null);

  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const w = el.offsetWidth;
    const h = el.offsetHeight;
    let left: number;
    let top: number;
    if ("x" in anchor) {
      left = anchor.x;
      top = anchor.y;
    } else {
      left = anchor.align === "end" ? anchor.rect.right - w : anchor.rect.left;
      top = anchor.above ? anchor.rect.top - h - 4 : anchor.rect.bottom + 4;
    }
    left = Math.max(8, Math.min(left, window.innerWidth - w - 8));
    top = Math.max(40, Math.min(top, window.innerHeight - h - 8));
    setPos({ left, top });
  }, [anchor]);

  useEffect(() => {
    const el = ref.current;
    el?.querySelector<HTMLElement>("button:not(:disabled), input")?.focus();
    const onDown = (e: PointerEvent) => {
      if (el && !el.contains(e.target as Node)) onClose();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        onClose();
      }
    };
    const onBlur = () => onClose();
    document.addEventListener("pointerdown", onDown, true);
    document.addEventListener("keydown", onKey, true);
    window.addEventListener("blur", onBlur);
    window.addEventListener("resize", onBlur);
    return () => {
      document.removeEventListener("pointerdown", onDown, true);
      document.removeEventListener("keydown", onKey, true);
      window.removeEventListener("blur", onBlur);
      window.removeEventListener("resize", onBlur);
    };
  }, [onClose]);

  return createPortal(
    <div
      ref={ref}
      className={className ? `popover ${className}` : "popover"}
      role={role}
      aria-label={label}
      onKeyDown={onKeyDown}
      onContextMenu={(e) => e.preventDefault()}
      style={pos ? { left: pos.left, top: pos.top } : { left: -9999, top: -9999 }}
    >
      {children}
    </div>,
    document.body,
  );
}

interface MenuProps {
  anchor: MenuAnchor;
  items: MenuItem[];
  onClose: () => void;
  label: string;
}

/** A keyboard-navigable menu of actions. */
export function Menu({ anchor, items, onClose, label }: MenuProps) {
  const onKeyDown = (e: ReactKeyboardEvent<HTMLDivElement>) => {
    if (e.key !== "ArrowDown" && e.key !== "ArrowUp") return;
    e.preventDefault();
    const buttons = Array.from(e.currentTarget.querySelectorAll<HTMLButtonElement>("button:not(:disabled)"));
    if (buttons.length === 0) return;
    const i = buttons.indexOf(document.activeElement as HTMLButtonElement);
    const next = e.key === "ArrowDown" ? (i + 1) % buttons.length : (i - 1 + buttons.length) % buttons.length;
    buttons[next]?.focus();
  };

  return (
    <Popover anchor={anchor} onClose={onClose} label={label} role="menu" className="menu" onKeyDown={onKeyDown}>
      {items.map((item, i) =>
        item === "separator" ? (
          <div key={`sep-${i}`} className="menu-sep" role="separator" />
        ) : (
          <button
            key={item.label}
            type="button"
            role="menuitem"
            className={item.danger ? "menu-item is-danger" : "menu-item"}
            disabled={item.disabled}
            onClick={() => {
              onClose();
              item.onSelect();
            }}
          >
            {item.icon}
            <span>{item.label}</span>
          </button>
        ),
      )}
    </Popover>
  );
}

/** State helper for a menu or popover opened from a click or a right click. */
export function useMenu(): {
  anchor: MenuAnchor | null;
  openAt: (e: ReactMouseEvent) => void;
  openBelow: (el: HTMLElement, align?: "start" | "end", above?: boolean) => void;
  close: () => void;
} {
  const [anchor, setAnchor] = useState<MenuAnchor | null>(null);
  const close = useRef(() => setAnchor(null)).current;
  return {
    anchor,
    openAt: (e) => {
      e.preventDefault();
      setAnchor({ x: e.clientX, y: e.clientY });
    },
    openBelow: (el, align = "start", above = false) => setAnchor({ rect: el.getBoundingClientRect(), align, above }),
    close,
  };
}
