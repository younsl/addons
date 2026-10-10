import { useEffect, useState, type ComponentProps, type ReactNode } from "react";
import { BellOff, Eye, Megaphone, Pencil } from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGemoji from "remark-gemoji";
import remarkGfm from "remark-gfm";
import { Announcement, api } from "@/api";
import { CopyIconButton } from "@/components/app-ui/copy-button";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Textarea } from "@/components/ui/textarea";
import { useDateTime, useTranslation } from "@/lib/i18n";

// How long "hide for a while" keeps the banner away. A changed announcement
// (different updated_at) reappears immediately regardless, so a snooze can
// never suppress a genuinely new notice.
const SNOOZE_DAYS = 7;
const SNOOZE_KEY = "forklift.announcement.snooze";

type Snooze = { until: number; updated_at: string };

function readSnooze(): Snooze | null {
  try {
    const raw = window.localStorage.getItem(SNOOZE_KEY);
    return raw ? (JSON.parse(raw) as Snooze) : null;
  } catch {
    return null;
  }
}

// snoozed reports whether the current announcement is inside an active snooze:
// the window has not elapsed and the content has not changed since it was set.
function snoozed(updatedAt: string): boolean {
  const s = readSnooze();
  return !!s && Date.now() < s.until && s.updated_at === updatedAt;
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
        remarkPlugins={[remarkGfm, remarkGemoji]}
        components={{ pre: CodeBlock, a: MarkdownLink }}
      >
        {source}
      </ReactMarkdown>
    </div>
  );
}

// AnnouncementBanner shows the site-wide notice (Jenkins system-message style)
// on the main page. Admins get an edit affordance with a write/preview dialog;
// everyone else only sees the rendered banner, and no banner means it renders
// nothing at all.
export function AnnouncementBanner({ isAdmin }: { isAdmin: boolean }) {
  const { t } = useTranslation();
  const fmtDate = useDateTime();
  const [announcement, setAnnouncement] = useState<Announcement | null>(null);
  const [editing, setEditing] = useState(false);
  const [hidden, setHidden] = useState(false);

  useEffect(() => {
    let cancelled = false;
    api
      .getAnnouncement()
      .then((a) => {
        if (!cancelled) {
          setAnnouncement(a);
          setHidden(snoozed(a.updated_at ?? ""));
        }
      })
      .catch(() => {
        // A failed fetch only hides the banner; the page stays usable.
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const body = announcement?.body ?? "";
  if (!body && !isAdmin) return null;
  if (announcement === null) return null;
  if (hidden && !isAdmin) return null;

  const snooze = () => {
    try {
      window.localStorage.setItem(SNOOZE_KEY, JSON.stringify({
        until: Date.now() + SNOOZE_DAYS * 24 * 60 * 60 * 1000,
        updated_at: announcement.updated_at ?? "",
      } satisfies Snooze));
    } catch {
      // Storage unavailable (private mode quota): hide for this page load only.
    }
    setHidden(true);
  };

  return (
    <>
      {body && hidden ? (
        // An admin's own snooze must not lock them out of editing, so the
        // hidden state keeps a minimal edit entry point.
        <div className="mb-4">
          <Button variant="outline" size="sm" onClick={() => setEditing(true)}>
            <Megaphone className="size-3.5" />
            {t("announce.edit")}
          </Button>
        </div>
      ) : body ? (
        <div className="mb-4 flex min-w-0 flex-col gap-2 rounded-lg border border-border bg-card px-4 py-3">
          <div className="flex min-w-0 items-start gap-3">
            <Megaphone className="mt-0.5 size-4 shrink-0 text-foreground" aria-hidden />
            <div className="min-w-0 flex-1">
              <Markdown source={body} />
            </div>
            {isAdmin && (
              <Button
                variant="ghost"
                size="icon"
                className="size-7 shrink-0 text-muted-foreground"
                aria-label={t("announce.edit")}
                onClick={() => setEditing(true)}
              >
                <Pencil className="size-3.5" />
              </Button>
            )}
          </div>
          {/* Meta row spans the full banner so the snooze action sits flush at
              the bottom-right corner regardless of content width. */}
          <div className="flex min-w-0 items-center justify-between gap-2">
            <p className="m-0 text-xs text-muted-foreground">
              {announcement.updated_at ? fmtDate(announcement.updated_at) : ""}
            </p>
            <Button
              variant="ghost"
              size="sm"
              className="-mb-1 -mr-2 h-6 gap-1.5 px-2 text-xs text-muted-foreground hover:text-foreground"
              onClick={snooze}
            >
              <BellOff className="size-3" aria-hidden />
              {t("announce.snooze")}
            </Button>
          </div>
        </div>
      ) : (
        <div className="mb-4">
          <Button variant="outline" size="sm" onClick={() => setEditing(true)}>
            <Megaphone className="size-3.5" />
            {t("announce.create")}
          </Button>
        </div>
      )}
      {isAdmin && editing && (
        <AnnouncementEditor
          announcement={announcement}
          onClose={() => setEditing(false)}
          onSaved={(a) => {
            setAnnouncement(a);
            setEditing(false);
            setHidden(false);
          }}
        />
      )}
    </>
  );
}

function AnnouncementEditor({
  announcement,
  onClose,
  onSaved,
}: {
  announcement: Announcement;
  onClose: () => void;
  onSaved: (a: Announcement) => void;
}) {
  const { t } = useTranslation();
  const fmtDate = useDateTime();
  const initial = announcement.body;
  const [draft, setDraft] = useState(initial);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState("");

  const save = (body: string) => {
    setSaving(true);
    setError("");
    api
      .putAnnouncement(body)
      .then(onSaved)
      .catch((e: Error) => setError(e.message))
      .finally(() => setSaving(false));
  };

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>{initial ? t("announce.edit") : t("announce.create")}</DialogTitle>
        </DialogHeader>
        <Tabs defaultValue="write">
          <TabsList
            variant="line"
            className="h-9 w-full justify-start gap-5 border-b border-border p-0"
          >
            <TabsTrigger value="write" className="flex-none px-1 pb-2">
              <Pencil className="size-3.5" />
              {t("announce.write")}
            </TabsTrigger>
            <TabsTrigger value="preview" className="flex-none px-1 pb-2">
              <Eye className="size-3.5" />
              {t("announce.preview")}
            </TabsTrigger>
          </TabsList>
          <TabsContent value="write">
            <Textarea
              value={draft}
              onChange={(e) => setDraft(e.target.value)}
              placeholder={t("announce.placeholder")}
              maxLength={MAX_CHARS}
              rows={10}
              className="min-h-[240px] font-mono text-sm break-all"
              autoFocus
            />
            <p
              className={`mt-1.5 text-right text-xs tabular-nums ${
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
          </TabsContent>
          <TabsContent value="preview">
            <div className="min-h-[240px] rounded-md border border-border bg-card p-3">
              {draft.trim() ? (
                <Markdown source={draft} />
              ) : (
                <p className="text-sm text-muted-foreground">{t("announce.empty-preview")}</p>
              )}
            </div>
          </TabsContent>
        </Tabs>
        <p className="text-xs text-muted-foreground">{t("announce.hint")}</p>
        {announcement.updated_by && announcement.updated_at && (
          <p className="text-xs text-muted-foreground">
            {t("announce.saved-by")}: {announcement.updated_by} ({fmtDate(announcement.updated_at)})
          </p>
        )}
        {error && <p className="text-sm text-destructive">{error}</p>}
        <div className="flex justify-between gap-2">
          {initial ? (
            <Button variant="outline" size="sm" disabled={saving} onClick={() => save("")}>
              {t("announce.clear")}
            </Button>
          ) : (
            <span />
          )}
          <div className="flex gap-2">
            <Button variant="ghost" size="sm" disabled={saving} onClick={onClose}>
              {t("announce.cancel")}
            </Button>
            <Button size="sm" disabled={saving} onClick={() => save(draft)}>
              {t("announce.save")}
            </Button>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}
