export const formatDateTime = (iso: string | null | undefined): string =>
  iso ? new Date(iso).toLocaleString() : 'Never';

export const formatDate = (iso: string | null | undefined): string =>
  iso ? new Date(iso).toLocaleDateString() : 'Never';

export const daysUntil = (iso: string): number =>
  Math.ceil((new Date(iso).getTime() - Date.now()) / 86_400_000);

export const formatRelative = (iso: string | null | undefined): string => {
  if (!iso) return 'never';
  const ms = Date.now() - new Date(iso).getTime();
  if (!Number.isFinite(ms)) return '';
  const minutes = Math.floor(ms / 60_000);
  if (minutes < 1) return 'just now';
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  if (days === 1) return '1d ago';
  return `${days}d ago`;
};

export const NAME_MAX = 100;
export const DESCRIPTION_MAX = 500;
/** Mirrors the backend rule: letters, digits, hyphen and underscore only. */
export const NAME_PATTERN = /^[A-Za-z0-9_-]+$/;

/** Returns a message when the name is unusable, or null when it is valid. */
export const nameError = (name: string): string | null => {
  if (name.length === 0) return 'Name is required.';
  if (name.length > NAME_MAX) return `Name must be at most ${NAME_MAX} characters.`;
  if (!NAME_PATTERN.test(name)) {
    return 'Only letters, digits, hyphens and underscores are allowed.';
  }
  return null;
};

/** Strips every character the name rule does not allow, so pasted text is cleaned in place. */
export const sanitizeName = (raw: string): string => raw.replace(/[^A-Za-z0-9_-]/g, '');
