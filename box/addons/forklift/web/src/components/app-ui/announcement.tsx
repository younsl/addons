import { useEffect, useRef, useState, type ComponentProps, type ReactNode, type Ref } from "react";
import { Clock, Megaphone, UserRound, X } from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGemoji from "remark-gemoji";
import remarkGfm from "remark-gfm";
import { remarkLineBreaks } from "@/lib/remark-line-breaks";
import { Announcement, api } from "@/api";
import { CopyIconButton } from "@/components/app-ui/copy-button";
import { DateTimeField } from "@/components/app-ui/date-time-field";
import { MarkdownEditor } from "@/components/app-ui/markdown-editor";
import { Button } from "@/components/ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { Switch } from "@/components/ui/switch";
import { useDateTime, useTranslation } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { formatAbsolute } from "@/utils/format-relative";
import { useAuth } from "@/authContext";

// A dismissal is remembered per browser against the notice's updated_at, so
// it lasts until the notice is changed, as on GitHub Enterprise Server: a new
// or edited notice shows again.
const DISMISS_KEY = "forklift.announcement.dismissed";

function dismissed(updatedAt: string): boolean {
  try {
    return window.localStorage.getItem(DISMISS_KEY) === updatedAt;
  } catch {
    return false;
  }
}

// Fixed character limit, mirrored by the API (maxAnnouncementChars). The
// textarea's maxLength hard-stops typing at the cap; the live counter warns
// from 90% so the limit never lands as a surprise.
const MAX_CHARS = 1000;

// extractText flattens a React node tree to the plain text it renders, which
// for a fenced code block is exactly the code the author wrote.
function extractText(node: ReactNode): string {
  if (typeof node === "string" || typeof node === "number") return String(node);
  if (Array.isArray(node)) return node.map(extractText).join("");
  if (node && typeof node === "object" && "props" in node) {
    return extractText((node as { props: { children?: ReactNode } }).props.children);
  }
  return "";
}

// CodeBlock wraps a fenced code block with a hover copy button so a command in
// an announcement (a mirror URL, a registry setting) can be taken verbatim.
function CodeBlock(props: ComponentProps<"pre">) {
  const { children, ...rest } = props;
  return (
    <div className="group/code relative">
      <pre {...rest}>{children}</pre>
      <CopyIconButton
        value={extractText(children).replace(/\n$/, "")}
        className="absolute right-1 top-1 bg-muted opacity-0 group-hover/code:opacity-100"
      />
    </div>
  );
}

// MarkdownLink styles announcement links distinctly (offset underline that
// strengthens on hover, monochrome to match the banner) and opens them in a
// new tab so a notice link never navigates the console away.
function MarkdownLink(props: ComponentProps<"a">) {
  const { children, ...rest } = props;
  return (
    <a
      {...rest}
      target="_blank"
      rel="noreferrer"
      className="break-all font-medium text-foreground underline decoration-foreground/40 underline-offset-2 transition-colors hover:decoration-foreground"
    >
      {children}
    </a>
  );
}

// Markdown renders through react-markdown, which never emits raw HTML from the
// source, so an announcement cannot inject markup. GFM adds tables/strikethrough
// and gemoji turns :tada:-style shortcodes into emoji.
function Markdown({ source }: { source: string }) {
  return (
    <div className="announcement-markdown min-w-0 text-sm leading-relaxed [overflow-wrap:anywhere] [&_blockquote]:border-l-2 [&_blockquote]:border-border [&_blockquote]:pl-3 [&_blockquote]:text-muted-foreground [&_code]:rounded [&_code]:bg-muted [&_code]:px-1 [&_code]:py-0.5 [&_code]:font-mono [&_code]:text-xs [&_h1]:mb-1.5 [&_h1]:mt-2 [&_h1]:text-xl [&_h1]:font-semibold [&_h1]:first:mt-0 [&_h2]:mb-1 [&_h2]:mt-2 [&_h2]:text-lg [&_h2]:font-semibold [&_h2]:first:mt-0 [&_h3]:mb-1 [&_h3]:mt-1.5 [&_h3]:text-base [&_h3]:font-medium [&_h3]:first:mt-0 [&_li]:ml-4 [&_ol]:list-decimal [&_p]:my-1 [&_pre]:overflow-x-auto [&_pre]:rounded [&_pre]:bg-muted [&_pre]:p-2 [&_table]:my-2 [&_td]:border [&_td]:border-border [&_td]:px-2 [&_td]:py-1 [&_th]:border [&_th]:border-border [&_th]:px-2 [&_th]:py-1 [&_ul]:list-disc">
      <ReactMarkdown
        remarkPlugins={[remarkGfm, remarkGemoji, remarkLineBreaks]}
        components={{ pre: CodeBlock, a: MarkdownLink }}
      >
        {source}
      </ReactMarkdown>
    </div>
  );
}

// AnnouncementBar shows the site-wide notice as one straight bar pinned across
// the top of the page, on every menu but Admin, with the whole Markdown always
// in view: the bar grows with the notice rather than hiding part of it. Administrators write it from
// Admin > Announcement; nothing to show, or a notice past its end, renders
// nothing.
export function AnnouncementBar() {
  const [announcement, setAnnouncement] = useState<Announcement | null>(null);
  const [hidden, setHidden] = useState(false);
  const [ended, setEnded] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let cancelled = false;
    api
      .getAnnouncement()
      .then((a) => {
        if (!cancelled) {
          setAnnouncement(a);
          setHidden(a.user_dismissible && dismissed(a.updated_at ?? ""));
        }
      })
      .catch(() => {
        // A failed fetch only hides the bar; the page stays usable.
      });
    return () => {
      cancelled = true;
    };
  }, []);

  // A notice that reaches its end while the page is open leaves on time.
  useEffect(() => {
    if (!announcement?.expires_at) return;
    const wait = new Date(announcement.expires_at).getTime() - Date.now();
    if (wait > 2 ** 31 - 1) return;
    const timer = window.setTimeout(() => setEnded(true), Math.max(0, wait));
    return () => window.clearTimeout(timer);
  }, [announcement?.expires_at]);

  const visible = Boolean(announcement?.active) && !hidden && !ended;

  // The page panel below fills the rest of the viewport, so the bar publishes
  // its height for the panel to subtract.
  useEffect(() => {
    const el = ref.current;
    if (!visible || !el) return;
    const publish = () =>
      document.documentElement.style.setProperty("--fx-announcement-h", `${el.offsetHeight}px`);
    publish();
    const observer = new ResizeObserver(publish);
    observer.observe(el);
    return () => {
      observer.disconnect();
      document.documentElement.style.removeProperty("--fx-announcement-h");
    };
  }, [visible]);

  if (!announcement || !visible) return null;

  const dismiss = () => {
    try {
      window.localStorage.setItem(DISMISS_KEY, announcement.updated_at ?? "");
    } catch {
      // Storage unavailable (private mode quota): hide for this page load only.
    }
    setHidden(true);
  };

  return (
    <AnnouncementBarView ref={ref} body={announcement.body} by={announcement.updated_by}
      at={announcement.updated_at} dismissible={announcement.user_dismissible}
      onDismiss={dismiss} className="sticky top-[var(--fx-impersonation-h,0px)] z-30" />
  );
}

// relativeTime says how long ago an instant was, in the console language.
function relativeTime(iso: string, language: string): string {
  const minutes = Math.round((new Date(iso).getTime() - Date.now()) / 60_000);
  const rtf = new Intl.RelativeTimeFormat(language === "ko" ? "ko" : "en", { numeric: "auto" });
  if (Math.abs(minutes) < 60) return rtf.format(minutes, "minute");
  const hours = Math.round(minutes / 60);
  if (Math.abs(hours) < 24) return rtf.format(hours, "hour");
  return rtf.format(Math.round(hours / 24), "day");
}

// AnnouncementBarView is the bar itself, shared by the live bar and the
// editor's preview so the preview is exactly what users will see.
function AnnouncementBarView({ ref, body, by, at, dismissible, onDismiss, className }: {
  ref?: Ref<HTMLDivElement>;
  body: string;
  by?: string;
  at?: string;
  dismissible: boolean;
  onDismiss?: () => void;
  className?: string;
}) {
  const { t, language } = useTranslation();
  return (
    <div ref={ref} role="status"
      className={cn("flex min-h-9 min-w-0 items-start gap-2 border-b border-[var(--fx-accent-ink)]/40 bg-[var(--fx-surface-panel)] px-[var(--fx-main-gutter-x)] py-1.5 text-sm max-lg:px-3 max-sm:px-2", className)}>
      <Megaphone className="mt-[3px] size-4 shrink-0 text-[var(--fx-accent-ink)]" aria-hidden />
      <div className="min-w-0 flex-1">
        {/* Who posted it and how long ago, as icons with the name and the age:
            the exact time is a hover away. */}
        <div className="flex flex-wrap items-center gap-x-3 gap-y-0.5 leading-[22px]">
          <span className="text-sm font-semibold text-foreground">{t("announce.title")}</span>
          {by && (
            <span className="inline-flex items-center gap-1 text-xs text-muted-foreground" aria-label={`${t("announce.posted-by")} ${by}`}>
              <UserRound className="size-3.5" aria-hidden />
              {by}
            </span>
          )}
          {at && (
            <Tooltip>
              <TooltipTrigger render={<span tabIndex={0} className="inline-flex items-center gap-1 text-xs text-muted-foreground" />}>
                <Clock className="size-3.5" aria-hidden />
                {relativeTime(at, language)}
              </TooltipTrigger>
              <TooltipContent>{formatAbsolute(at, language)}</TooltipContent>
            </Tooltip>
          )}
        </div>
        <div className="[&_p]:!my-0">
          <Markdown source={body} />
        </div>
      </div>
      {/* Only an administrator decides whether a notice can be dismissed. */}
      {dismissible && (
        <Tooltip>
          <TooltipTrigger render={<Button variant="ghost" size="icon-xs" className="mt-px shrink-0 text-muted-foreground hover:text-foreground" aria-label={t("announce.dismiss")} onClick={onDismiss} />}>
            <X aria-hidden />
          </TooltipTrigger>
          <TooltipContent>{t("announce.dismiss")}</TooltipContent>
        </Tooltip>
      )}
    </div>
  );
}

const tomorrowMorning = () => {
  const d = new Date();
  d.setDate(d.getDate() + 1);
  d.setHours(9, 0, 0, 0);
  return d;
};

const startOfToday = () => {
  const d = new Date();
  d.setHours(0, 0, 0, 0);
  return d;
};

// AnnouncementForm is the Admin > Announcement page body: write the notice in
// Markdown, preview it as the banner will render it, then save or clear it.
export function AnnouncementForm() {
  const { t } = useTranslation();
  const { me } = useAuth();
  // The preview is posted by whoever is editing, as of now.
  const [previewAt] = useState(() => new Date().toISOString());
  const fmtDate = useDateTime();
  const [announcement, setAnnouncement] = useState<Announcement | null>(null);
  const [draft, setDraft] = useState("");
  // The end in local time, null while none is set or it is half typed.
  const [ends, setEnds] = useState<Date | null>(null);
  const [endsOn, setEndsOn] = useState(false);
  const [dismissible, setDismissible] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState("");
  const [saved, setSaved] = useState(false);

  useEffect(() => {
    let cancelled = false;
    api
      .getAnnouncement()
      .then((a) => {
        if (!cancelled) {
          setAnnouncement(a);
          setDraft(a.body);
          setEnds(a.expires_at ? new Date(a.expires_at) : null);
          setEndsOn(Boolean(a.expires_at));
          setDismissible(a.user_dismissible);
        }
      })
      .catch((e: Error) => !cancelled && setError(e.message));
    return () => {
      cancelled = true;
    };
  }, []);

  const save = (body: string, expiresAt: string | null) => {
    setSaving(true);
    setError("");
    setSaved(false);
    api
      .putAnnouncement(body, expiresAt, body === "" ? false : dismissible)
      .then((a) => {
        setAnnouncement(a);
        setDraft(a.body);
        setEnds(a.expires_at ? new Date(a.expires_at) : null);
        setEndsOn(Boolean(a.expires_at));
        setDismissible(a.user_dismissible);
        setSaved(true);
      })
      .catch((e: Error) => setError(e.message))
      .finally(() => setSaving(false));
  };

  const clearing = draft.trim() === "";
  const endsAt = endsOn ? ends : null;
  const endInvalid = endsOn && (!endsAt || endsAt.getTime() <= Date.now());
  const endPast = endsAt !== null && endsAt.getTime() <= Date.now();
  const unchanged = announcement !== null
    && draft === announcement.body
    && (endsAt?.toISOString() ?? null) === (announcement.expires_at ? new Date(announcement.expires_at).toISOString() : null)
    && dismissible === announcement.user_dismissible;

  if (!announcement) {
    return error
      ? <p className="text-sm text-destructive">{error}</p>
      : <p className="text-sm text-muted-foreground">{t("common.loading")}</p>;
  }

  return (
    <div className="flex flex-col gap-3">
      {saved && (
        <div className="rounded-md border border-[var(--fx-success)]/50 bg-[var(--fx-success)]/10 px-3 py-2 text-sm">
          {announcement.body ? t("announce.saved") : t("announce.cleared")}
        </div>
      )}
      {/* The preview is the live bar itself, redrawn on every keystroke, so
          what is saved is exactly what users will see. */}
      <section className="flex flex-col gap-1.5">
        <h3 className="m-0 flex items-center gap-2 text-sm font-medium">
          {t("announce.preview")}
          <span className="rounded border border-border px-1.5 text-[11px] font-normal leading-5 text-muted-foreground">{t("announce.not-published")}</span>
        </h3>
        <p className="m-0 text-xs text-muted-foreground">{t("announce.preview-hint")}</p>
        {draft.trim() ? (
          <AnnouncementBarView body={draft} by={me.username} at={previewAt} dismissible={dismissible}
            className="overflow-hidden rounded-md border border-border" />
        ) : (
          <p className="m-0 rounded-md border border-dashed border-border px-3 py-2 text-sm text-muted-foreground">
            {announcement.body ? t("announce.empty-takes-down") : t("announce.empty-preview")}
          </p>
        )}
      </section>
      <section className="mt-2 flex flex-col gap-1.5">
        <h3 className="m-0 text-sm font-medium">{t("announce.write")}</h3>
        <p className="m-0 text-xs text-muted-foreground">{t("announce.write-hint")}</p>
        <MarkdownEditor
          value={draft}
          onChange={(next) => {
            setDraft(next);
            setSaved(false);
          }}
          placeholder={t("announce.placeholder")}
          maxLength={MAX_CHARS}
        />
        <p
          className={`m-0 text-right text-xs tabular-nums ${
            draft.length >= MAX_CHARS
              ? "text-destructive"
              : draft.length >= MAX_CHARS * 0.9
                ? "text-accent-ink"
                : "text-muted-foreground"
          }`}
          aria-live="polite"
        >
          {draft.length.toLocaleString()} / {MAX_CHARS.toLocaleString()}
        </p>
      </section>
      {/* Each option reads as its title, what it does, then its control, with
          room between options so they do not run together. */}
      <div className="mt-2 flex flex-col gap-6">
        <section className="flex flex-col gap-1.5">
          <h3 id="announce-ends-title" className="m-0 text-sm font-medium">{t("announce.ends")}</h3>
          <p className="m-0 text-xs text-muted-foreground">{t("announce.ends-hint")}</p>
          <Switch aria-labelledby="announce-ends-title" checked={endsOn} className="mt-1"
            onCheckedChange={(v) => {
              setEndsOn(Boolean(v));
              // Turning the end on starts from tomorrow morning rather than
              // an empty field.
              if (v && (!ends || ends.getTime() <= Date.now())) setEnds(tomorrowMorning());
              setSaved(false);
            }} />
          {endsOn && (
            <div className="mt-1.5 flex flex-col gap-1">
              <DateTimeField value={ends} invalid={endPast} minDate={startOfToday()}
                onChange={(next) => {
                  setEnds(next);
                  setSaved(false);
                }} />
              {endPast && <p className="m-0 text-xs text-destructive">{t("announce.ends-past")}</p>}
            </div>
          )}
        </section>
        <section className="flex flex-col gap-1.5">
          <h3 id="announce-dismissible-title" className="m-0 text-sm font-medium">{t("announce.dismissible")}</h3>
          <p className="m-0 text-xs text-muted-foreground">{t("announce.dismissible-hint")}</p>
          <Switch aria-labelledby="announce-dismissible-title" checked={dismissible} className="mt-1"
            onCheckedChange={(v) => {
              setDismissible(Boolean(v));
              setSaved(false);
            }} />
        </section>
        {announcement.body && announcement.expires_at && announcement.active === false && (
          <p className="m-0 text-sm text-[var(--fx-warning)]">
            {t("announce.ended").replace("{date}", announcement.expires_at ? fmtDate(announcement.expires_at) : "")}
          </p>
        )}
      </div>
      {announcement.updated_by && announcement.updated_at && (
        <p className="m-0 mt-2 text-xs text-muted-foreground">
          {t("announce.saved-by")}: {announcement.updated_by} ({fmtDate(announcement.updated_at)})
        </p>
      )}
      {error && <p className="m-0 text-sm text-destructive">{error}</p>}
      {/* Emptying the text and saving is how a notice is taken down; there is
          no separate clear action. An empty notice has no end to check. */}
      <div className="flex justify-end">
        <Button disabled={saving || unchanged || (!clearing && endInvalid)}
          onClick={() => (clearing ? save("", null) : save(draft, endsAt ? endsAt.toISOString() : null))}>
          {t("announce.save")}
        </Button>
      </div>
    </div>
  );
}
