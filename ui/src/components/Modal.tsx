// Modal dialog on the native <dialog> element (focus trap, Escape, top layer for free).

import { useEffect, useRef, type FormEvent, type ReactNode } from "react";

interface ModalProps {
  /** The heading (peer-chosen names in it go in a <bdi>). */
  title: ReactNode;
  onClose: () => void;
  children: ReactNode;
  /** Buttons, right-aligned at the bottom. */
  footer?: ReactNode;
  /** Makes the body a form; Enter in a field submits. */
  onSubmit?: () => void;
  wide?: boolean;
  className?: string;
}

/** A centred modal dialog. Closes on Escape and on a click outside. */
export function Modal({ title, onClose, children, footer, onSubmit, wide, className }: ModalProps) {
  const ref = useRef<HTMLDialogElement>(null);

  useEffect(() => {
    const d = ref.current;
    if (d && !d.open) {
      d.showModal();
      // showModal focuses the first focusable element; prefer an explicit target or the first field
      const target =
        d.querySelector<HTMLElement>("[data-autofocus]") ??
        d.querySelector<HTMLElement>("input:not([readonly]):not([type='checkbox']):not([type='range']), textarea");
      target?.focus();
    }
    return () => {
      if (d?.open) d.close();
    };
  }, []);

  const submit = (e: FormEvent) => {
    e.preventDefault();
    onSubmit?.();
  };

  const body = (
    <>
      <div className="modal-head">
        <h2 id="modal-title">{title}</h2>
      </div>
      <div className="modal-body">{children}</div>
      {footer && <div className="modal-foot">{footer}</div>}
    </>
  );

  return (
    <dialog
      ref={ref}
      className={["modal", wide ? "modal-wide" : "", className ?? ""].filter(Boolean).join(" ")}
      aria-labelledby="modal-title"
      onCancel={(e) => {
        e.preventDefault();
        onClose();
      }}
      onMouseDown={(e) => {
        if (e.target === ref.current) onClose();
      }}
    >
      {onSubmit ? (
        <form className="modal-inner" onSubmit={submit}>
          {body}
        </form>
      ) : (
        <div className="modal-inner">{body}</div>
      )}
    </dialog>
  );
}
