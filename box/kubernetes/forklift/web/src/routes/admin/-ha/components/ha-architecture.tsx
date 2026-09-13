import { useNavigate } from "@tanstack/react-router";

import { useTranslation } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { USAGE_CRITICAL_PCT, USAGE_WARNING_PCT, usageTone } from "@/lib/usage";
import { formatFileSize } from "@/utils/format-file-size";

import type { StorageUsage } from "@/routes/admin/-ha/utils/ha-status";
import type { HAStatus } from "@/services/v1/openapi-types";

// HAArchitecture renders the live active/standby topology as an SVG: client →
// Service → the active leader pod, the standby kept Ready alongside, the election
// Lease linking the two, and the shared storage the single writer owns. It tracks
// the live status, so on a failover the ACTIVE/STANDBY roles swap on the next
// poll. In single-instance mode (election disabled) only the lone active pod is
// drawn. Active elements are green; the standby and its paths are dashed/dimmed.
export function HaArchitecture({ status, usage }: { status: HAStatus; usage: StorageUsage | null }) {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const ha = status.enabled;
  const s3 = status.backend === "s3";
  // The storage box grows to hold the utilization bar when there is a capacity
  // to show (PersistentVolume or MinIO cluster).
  const usagePct = usage ? Math.min(100, Math.max(0, usage.ratio * 100)) : null;
  const storageH = usagePct === null ? 88 : 128;
  // Green below the warning mark, amber to the critical one, red above it.
  const usageFill = usagePct === null ? "" : {
    critical: "fill-[var(--fx-severity-critical)]",
    warning: "fill-[var(--fx-warning)]",
    ok: "fill-[var(--fx-success)]",
  }[usageTone(usagePct)];
  const showFencing = s3 && typeof status.fencing_token === "number" && status.fencing_token > 0;
  const trunc = (s: string, n = 20) => (s.length > n ? s.slice(0, n - 1) + "…" : s);

  const activeName = status.leader || status.identity || "-";
  const activeThis = status.is_leader;
  const standbyName = status.is_leader ? "standby peer" : status.identity || "-";
  const standbyThis = !status.is_leader;
  // Pod sub-line: this pod's forklift version, plus a "this pod" marker on the
  // box we are connected to. (Only this pod's version is known to the API.)
  const ver = status.version ? (/^\d/.test(status.version) ? `v${status.version}` : status.version) : "";
  const podSub = (thisPod: boolean) => [ver, thisPod ? "this pod" : ""].filter(Boolean).join(" · ");
  // Active pod sits at the top when a standby is drawn below it; centred when
  // alone (single instance). The whole flow reads left to right.
  const apY = ha ? 38 : 122;
  const apCy = apY + 46;
  const boxClass = "fill-[var(--input-bg)] stroke-border stroke-[1.5]";
  const activeBoxClass = "stroke-[var(--fx-success)]";
  const standbyBoxClass = "[stroke-dasharray:6_4]";
  const edgeClass = "fill-none stroke-muted-foreground stroke-[1.5]";
  const activeEdgeClass = "stroke-[var(--fx-success)]";
  const dimEdgeClass = "opacity-50 [stroke-dasharray:6_4]";
  const flowEdgeClass = "[stroke-dasharray:7_5] [animation:ha-edge-flow_0.7s_linear_infinite] motion-reduce:animate-none motion-reduce:[stroke-dasharray:none]";
  const labelClass = "fill-foreground text-[13px]";
  const monoLabelClass = "fill-foreground font-mono text-xs";
  const subLabelClass = "fill-muted-foreground text-[11px]";
  const tagClass = "text-[11px] tracking-[0.06em]";

  return (
    <div className="mt-3 w-full overflow-x-auto">
      <div className="mb-1.5 text-xs text-muted-foreground">{t("ha.topology")}</div>
      <svg className="block h-auto w-full min-w-[720px]" viewBox="0 0 1080 340" role="img"
        aria-label={t("ha.diagram-label")}>
        <defs>
          <marker id="ha-head" viewBox="0 0 8 8" markerWidth="7" markerHeight="7" refX="6.5" refY="4" orient="auto-start-reverse">
            <path className="fill-muted-foreground" d="M0,1 L7,4 L0,7 Z" />
          </marker>
          <marker id="ha-head-ok" viewBox="0 0 8 8" markerWidth="7" markerHeight="7" refX="6.5" refY="4" orient="auto-start-reverse">
            <path className="fill-[var(--fx-success)]" d="M0,1 L7,4 L0,7 Z" />
          </marker>
        </defs>

        {/* Client */}
        <rect className={boxClass} x="20" y="140" width="150" height="56" rx="8" />
        <text className={labelClass} x="95" y="164" textAnchor="middle">{t("ha.client")}</text>
        <text className={subLabelClass} x="95" y="181" textAnchor="middle">{t("ha.package-managers")}</text>

        {/* Service */}
        <rect className={boxClass} x="250" y="134" width="180" height="68" rx="8" />
        <text className={labelClass} x="340" y="164" textAnchor="middle">{t("ha.service")}</text>
        <text className={subLabelClass} x="340" y="182" textAnchor="middle">{t("ha.routes-to-leader")}</text>

        {/* Client -> Service */}
        <line className={cn(edgeClass, flowEdgeClass)} x1="170" y1="168" x2="250" y2="168" markerEnd="url(#ha-head)" />
        <text className={subLabelClass} x="210" y="160" textAnchor="middle">HTTP</text>

        {/* Active pod - crowned, since this box is always the active leader. */}
        <rect className={cn(boxClass, activeBoxClass)} x="540" y={apY} width="230" height="92" rx="8" />
        <path className="fill-[var(--fx-success)]"
          d={`M644,${apY + 17} L644,${apY + 7} L649.5,${apY + 12} L655,${apY + 5} L660.5,${apY + 12} L666,${apY + 7} L666,${apY + 17} Z`} />
        <text className={cn(tagClass, "fill-[var(--fx-success)]")} x="655" y={apY + 33} textAnchor="middle">
          {ha ? t("ha.active-leader") : t("ha.active-single")}
        </text>
        <text className={monoLabelClass} x="655" y={apY + 52} textAnchor="middle"><title>{activeName}</title>{trunc(activeName, 24)}</text>
        {podSub(activeThis) && <text className={subLabelClass} x="655" y={apY + 72} textAnchor="middle">{podSub(activeThis)}</text>}

        {/* Service -> Active (active path) */}
        <line className={cn(edgeClass, activeEdgeClass, flowEdgeClass)} x1="430" y1={ha ? 150 : 168} x2="540" y2={apCy} markerEnd="url(#ha-head-ok)" />

        {/* Active -> Storage (single writer) */}
        <line className={cn(edgeClass, activeEdgeClass, flowEdgeClass)} x1="770" y1={apCy} x2="900" y2={ha ? 150 : 168} markerEnd="url(#ha-head-ok)" />
        <text className={subLabelClass} x="835" y={ha ? 108 : 160} textAnchor="middle">{t("ha.single-writer")}</text>

        {ha && (
          <>
            {/* Standby pod */}
            <rect className={cn(boxClass, standbyBoxClass)} x="540" y="212" width="230" height="92" rx="8" />
            <text className={cn(tagClass, "fill-muted-foreground")} x="655" y="240" textAnchor="middle">{t("ha.standby")}</text>
            <text className={monoLabelClass} x="655" y="264" textAnchor="middle"><title>{standbyName}</title>{trunc(standbyName, 24)}</text>
            {podSub(standbyThis) && <text className={subLabelClass} x="655" y="284" textAnchor="middle">{podSub(standbyThis)}</text>}

            {/* Service -> Standby (ready, not served) */}
            <line className={cn(edgeClass, dimEdgeClass)} x1="430" y1="186" x2="540" y2="258" markerEnd="url(#ha-head)" />
            <text className={subLabelClass} x="485" y="232" textAnchor="middle">{t("ha.ready")}</text>

            {/* Lease link between the two pods (vertical) */}
            <line className={edgeClass} x1="655" y1="132" x2="655" y2="210" markerStart="url(#ha-head)" markerEnd="url(#ha-head)" />
            <text className={subLabelClass} x="678" y="166" textAnchor="start">{t("ha.lease")}</text>
            <text className={subLabelClass} x="678" y="182" textAnchor="start">{t("ha.leader-election-caption")}</text>

            {/* Standby -> Storage */}
            <line className={cn(edgeClass, dimEdgeClass)} x1="770" y1="258" x2="900" y2="186" markerEnd="url(#ha-head)" />
            <text className={subLabelClass} x="835" y="232" textAnchor="middle">{s3 ? "syncs" : "standby"}</text>
          </>
        )}

        {/* Storage - the one box with somewhere to go: the Storage page carries
            the full capacity, drive-health and consistency detail this summarizes. */}
        <g
          className="group cursor-pointer outline-none"
          role="link"
          tabIndex={0}
          aria-label={t("ha.open-storage")}
          onClick={() => navigate({ to: "/admin/storage" })}
          onKeyDown={(e) => {
            if (e.key !== "Enter" && e.key !== " ") return;
            e.preventDefault();
            navigate({ to: "/admin/storage" });
          }}
        >
          <title>{t("ha.open-storage")}</title>
          {/* Keyboard focus gets a ring outside the box; hover does not, so
              pointing at the diagram stays quiet. Both are drawn outside the
              border, so nothing moves when they appear. */}
          <rect
            className="fill-none stroke-transparent stroke-[1.5] transition-[stroke] duration-150 group-focus-visible:stroke-[color-mix(in_oklch,var(--fx-accent-ink)_65%,transparent)]"
            x="895" y="119" width="180" height={storageH + 10} rx="11"
          />
          <rect
            className={cn(boxClass, "transition-[fill,stroke] duration-150 group-hover:fill-[var(--fx-surface-hover)] group-hover:stroke-[var(--fx-border-strong)]")}
            x="900" y="124" width="170" height={storageH} rx="8"
          />
          <text className={labelClass} x="985" y="158" textAnchor="middle">{s3 ? "Object Storage" : "Block Storage"}</text>
          <text className={cn(subLabelClass, "font-mono")} x="985" y="177" textAnchor="middle">
            <title>{status.storage_endpoint || "-"}</title>{trunc(status.storage_endpoint || "-", 24)}
          </text>
          <text className={subLabelClass} x="985" y="194" textAnchor="middle">
            {s3 ? (showFencing ? `fenced · token ${status.fencing_token}` : "fenced writes") : t("ha.single-writer")}
          </text>
          {/* Utilization of the volume (PersistentVolume) or MinIO cluster. Plain
              AWS S3 has no capacity to fill, so it carries no bar. */}
          {usagePct !== null && usage && (
            <>
              <text className={subLabelClass} x="916" y="213" textAnchor="start">{t("ha.storage-usage")}</text>
              <text className={cn(subLabelClass, "fill-foreground tabular-nums")} x="1054" y="213" textAnchor="end">
                {usagePct.toFixed(1)}%
              </text>
              <rect className="fill-border" x="916" y="219" width="138" height="6" rx="3" />
              <rect className={usageFill} x="916" y="219" width={(138 * usagePct) / 100} height="6" rx="3" />
              {/* Threshold marks, drawn over the fill so the warning and critical
                  bands are readable at any level: a bar at 40% still shows how
                  much headroom is left before each one. The notch cuts the bar,
                  the pointer below names the exact spot. */}
              {[
                { pct: USAGE_WARNING_PCT, fill: "fill-[var(--fx-warning)]" },
                { pct: USAGE_CRITICAL_PCT, fill: "fill-[var(--fx-severity-critical)]" },
              ].map((mark) => {
                const x = 916 + (138 * mark.pct) / 100;
                return (
                  <g key={mark.pct}>
                    <rect className="fill-[var(--input-bg)]" x={x - 0.75} y="219" width="1.5" height="6" />
                    <path className={mark.fill} d={`M${x - 3},231 L${x + 3},231 L${x},226 Z`} />
                  </g>
                );
              })}
              <text className={subLabelClass} x="985" y="240" textAnchor="middle">
                {formatFileSize(usage.usedBytes)} / {formatFileSize(usage.totalBytes)}
              </text>
            </>
          )}
        </g>
      </svg>
    </div>
  );
}
