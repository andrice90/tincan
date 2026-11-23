// Small shared building blocks: avatars, icon buttons.

import { memo, type ReactNode } from "react";
import type { UserId } from "../api/types";
import { hashIndex, initials } from "../lib/format";
import { useSpeaking } from "../store/store";

interface AvatarProps {
  userId: UserId;
  name: string;
  /** Pixel size; one of 20, 24, 32, 40, 64. */
  size?: number;
  /** Shows the online marker when true and the offline look when false. Omit to show neither. */
  online?: boolean;
  speaking?: boolean;
}

/** A round avatar with initials on a colour derived from the user id. */
export const Avatar = memo(function Avatar({ userId, name, size = 32, online, speaking = false }: AvatarProps) {
  const cls = ["avatar", `avatar-${size}`, speaking ? "is-speaking" : "", online === false ? "is-offline" : ""]
    .filter(Boolean)
    .join(" ");
  return (
    <span className={cls} style={{ background: `var(--avatar-${hashIndex(userId, 8)})` }} aria-hidden="true">
      {initials(name)}
      {online === true && <span className="avatar-online" />}
    </span>
  );
});

/** An avatar whose ring follows that user's live speaking flag. Only this re-renders on speaking changes. */
export function SpeakingAvatar(props: Omit<AvatarProps, "speaking">) {
  const speaking = useSpeaking(props.userId);
  return <Avatar {...props} speaking={speaking} />;
}

interface IconButtonProps {
  label: string;
  onClick?: () => void;
  children: ReactNode;
  pressed?: boolean;
  disabled?: boolean;
  /** "off" colours the icon red (muted mic), "danger" for destructive actions. */
  tone?: "off" | "danger";
  small?: boolean;
  className?: string;
  /** Tooltip text when it should differ from the label. */
  tooltip?: string;
}

/** An icon-only button with an accessible name and a native tooltip. */
export function IconButton({ label, onClick, children, pressed, disabled, tone, small, className, tooltip }: IconButtonProps) {
  const cls = [
    "icon-btn",
    small ? "icon-btn-sm" : "",
    tone === "off" ? "is-off" : "",
    tone === "danger" ? "is-danger" : "",
    className ?? "",
  ]
    .filter(Boolean)
    .join(" ");
  return (
    <button
      type="button"
      className={cls}
      aria-label={label}
      title={tooltip ?? label}
      aria-pressed={pressed}
      disabled={disabled}
      onClick={onClick}
    >
      {children}
    </button>
  );
}

/** Lucide icon size used across the app. */
export const ICON = 18;
/** Smaller icon size for dense rows. */
export const ICON_SM = 16;
