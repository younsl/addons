import { CopyButton } from "@/components/app-ui/copy-button";
import { MrkdwnSource } from "@/components/app-ui/mrkdwn-source";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { useTranslation } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { renderMrkdwn } from "@/routes/workspace/repositories/-components/detail/policy-flow";

// AlarmPreview shows an alarm's mrkdwn text two ways: Slack renders it as a
// channel shows it, and Raw is the same source unrendered, exactly as sent,
// with a button that copies it.
export function AlarmPreview({ text, className }: { text: string; className?: string }) {
  const { t } = useTranslation();
  return (
    <Tabs defaultValue="slack" className={cn("min-w-0", className)}>
      <TabsList variant="line" className="h-8 w-full justify-start gap-4 border-b border-border p-0">
        <TabsTrigger value="slack" className="flex-none px-1 pb-2">{t("insights.alarm-tab-slack")}</TabsTrigger>
        <TabsTrigger value="raw" className="flex-none px-1 pb-2">{t("insights.alarm-tab-raw")}</TabsTrigger>
      </TabsList>
      <TabsContent value="slack" className="min-w-0">
        <div className="max-h-72 overflow-auto break-words rounded-md border border-border bg-card p-3 text-sm leading-relaxed">
          {renderMrkdwn(text)}
        </div>
      </TabsContent>
      {/* The copy button sits in the viewer's corner, outside its scroll, so it
          stays put while the source scrolls under the reserved right margin. */}
      <TabsContent value="raw" className="relative min-w-0">
        <MrkdwnSource text={text} className="max-h-72 min-w-0 pr-10" />
        <CopyButton value={text} className="absolute right-1.5 top-1.5" />
      </TabsContent>
    </Tabs>
  );
}
