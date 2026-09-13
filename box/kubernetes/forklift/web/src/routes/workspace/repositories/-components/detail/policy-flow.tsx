import { DndContext, DragEndEvent, KeyboardSensor, PointerSensor, closestCenter, useSensor, useSensors } from "@dnd-kit/core";
import { SortableContext, arrayMove, sortableKeyboardCoordinates, useSortable, verticalListSortingStrategy } from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import {
  Handle,
  MarkerType,
  Position,
  ReactFlow,
  type Edge as FlowEdge,
  type Node as FlowNode,
  type NodeProps,
  type ReactFlowInstance,
} from "@xyflow/react";
import { GripVertical, LockKeyhole } from "lucide-react";
import { ReactNode, useEffect, useRef, useState } from "react";
import { PolicyName } from "@/api";
import { Card, CardContent } from "@/components/ui/card";
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import type { Repository } from "@/services/v1/openapi-types";
import { useTranslation } from "@/lib/i18n";
import type { RepositoryDraftSetter } from "@/routes/workspace/repositories/-hooks/use-repository-draft";
import { cn } from "@/lib/utils";
import "@xyflow/react/dist/style.css";

// LinesInput is a textarea for "one value per line" lists. It keeps the raw text
// in local state so a blank line - e.g. the one created by pressing Enter - is
// not swallowed mid-edit, and reports the trimmed, non-empty lines to onChange.
export function LinesInput({ value, onChange, rows = 3, placeholder }: {
  value: string[];
  onChange: (lines: string[]) => void;
  rows?: number;
  placeholder?: string;
}) {
  const joined = value.join("\n");
  const [text, setText] = useState(joined);
  // Re-sync only when the external value diverges from what the current text
  // parses to (e.g. the repository changed), never on plain keystrokes.
  useEffect(() => {
    if (text.split("\n").map((s) => s.trim()).filter(Boolean).join("\n") !== joined) setText(joined);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [joined]);
  return (
    <Textarea rows={rows} placeholder={placeholder} value={text}
      onChange={(e) => {
        setText(e.target.value);
        onChange(e.target.value.split("\n").map((s) => s.trim()).filter(Boolean));
      }} />
  );
}

// renderMrkdwn renders a Slack/Mattermost mrkdwn string (the exact alarm text)
// as React: *bold* becomes bold, <url|label> becomes a link, and newlines break
// lines - so the notification preview shows the real template as it would appear.
export function renderMrkdwn(text: string): ReactNode {
  return text.split("\n").map((line, li) => {
    const parts: ReactNode[] = [];
    const re = /\*([^*]+)\*|<([^|>]+)\|([^>]+)>/g;
    let last = 0, m: RegExpExecArray | null, k = 0;
    while ((m = re.exec(line)) !== null) {
      if (m.index > last) parts.push(line.slice(last, m.index));
      if (m[1] !== undefined) parts.push(<strong key={k++}>{m[1]}</strong>);
      else {
        // Only allow http(s) hrefs; anything else (javascript:, data:) is dropped
        // to plain text so a crafted link in the payload can't inject a scheme.
        const safe = /^https?:\/\//i.test(m[2]) ? m[2] : undefined;
        parts.push(safe
          ? <a key={k++} href={safe} target="_blank" rel="noreferrer noopener" className="text-accent-ink underline">{m[3]}</a>
          : <span key={k++}>{m[3]}</span>);
      }
      last = re.lastIndex;
    }
    if (last < line.length) parts.push(line.slice(last));
    return <div key={li}>{parts.length ? parts : " "}</div>;
  });
}

export const defaultPolicyOrder: PolicyName[] = ["vulnerability", "license", "age"];

export function effectivePolicyOrder(repo: Repository): PolicyName[] {
  const order = (repo.config.policy_pipeline?.order ?? []).filter(
    (name): name is PolicyName => defaultPolicyOrder.includes(name as PolicyName),
  );
  if (!order || order.length !== defaultPolicyOrder.length || !defaultPolicyOrder.every((name) => order.includes(name))) {
    return [...defaultPolicyOrder];
  }
  return [...order];
}

function SortablePolicyRow({
  policy,
  canWrite,
  dragLabel,
  className,
  selected,
  children,
}: {
  policy: PolicyName;
  canWrite: boolean;
  dragLabel: string;
  className: string;
  selected: boolean;
  children: ReactNode;
}) {
  const { attributes, listeners, setNodeRef, setActivatorNodeRef, transform, transition, isDragging, isOver } = useSortable({
    id: policy,
    disabled: !canWrite,
  });

  return (
    <div
      ref={setNodeRef}
      style={{ transform: CSS.Transform.toString(transform), transition }}
      className={cn(
        "relative select-none",
        className,
        selected && "border-[var(--border-strong)] bg-[var(--panel-3)]",
        isDragging && "z-10 opacity-55 shadow-lg",
        isOver && !isDragging && "ring-2 ring-accent-ink ring-offset-2 ring-offset-background",
      )}
    >
      <button
        ref={setActivatorNodeRef}
        type="button"
        {...attributes}
        {...listeners}
        disabled={!canWrite}
        title={dragLabel}
        aria-label={dragLabel}
        className="flex size-8 shrink-0 touch-none cursor-grab items-center justify-center rounded-[var(--radius)] text-muted-foreground outline-none hover:bg-muted hover:text-foreground focus-visible:ring-2 focus-visible:ring-accent-ink disabled:cursor-not-allowed disabled:opacity-40 active:cursor-grabbing"
        onClick={(event) => event.stopPropagation()}
      >
        <GripVertical className="size-4" aria-hidden="true" />
      </button>
      {children}
    </div>
  );
}

type PolicySeverity = "block" | "warn" | "audit";

type PolicyDisplayNode = {
  name: string;
  label: string;
  enabled: boolean;
  severity: PolicySeverity;
  action: string;
  policy?: PolicyName;
  badge?: number;
};

type PipelineGraphData = {
  label: string;
  stage: string;
  marker: string;
  enabled: boolean;
  severity: PolicySeverity;
  action: string;
  vertical: boolean;
  fixed?: boolean;
};

type PipelineGraphNodeType = FlowNode<PipelineGraphData, "pipeline">;

function PipelineGraphNode({ data }: NodeProps<PipelineGraphNodeType>) {
  const direction = data.vertical ? Position.Top : Position.Left;
  const output = data.vertical ? Position.Bottom : Position.Right;
  const accentClass = data.enabled
    ? data.severity === "block"
      ? "bg-[var(--danger)]"
      : data.severity === "warn"
        ? "bg-[var(--accent)]"
        : "bg-[var(--fx-info)]"
    : "bg-[var(--border-strong)]";
  return (
    <div className={cn(
      "policy-flow-node relative flex h-[162px] w-[288px] flex-col justify-center overflow-hidden rounded-[var(--radius)] border border-[var(--border)] px-7 shadow-[var(--fx-panel-highlight)]",
      data.fixed ? "bg-[var(--panel-2)]" : "bg-[var(--panel-3)]",
      data.enabled && "policy-flow-node-active",
    )}>
      <Handle
        type="target"
        position={direction}
        className="!size-1 !border-0 !bg-transparent !opacity-0"
      />
      <div className="mb-4 flex min-w-0 items-center justify-between gap-3">
        <span className="truncate text-[12px] font-semibold text-[var(--muted)] uppercase">{data.stage}</span>
        {data.fixed ? (
          <span className="inline-flex shrink-0 items-center text-[11px] font-semibold text-[var(--muted)]" title={data.marker}>
            <LockKeyhole className="size-4" aria-hidden="true" />
          </span>
        ) : (
          <span className="inline-flex size-7 shrink-0 items-center justify-center rounded-full border border-[var(--border-strong)] text-[11px] font-bold text-[var(--text)] tabular-nums">
            {data.marker}
          </span>
        )}
      </div>
      <span className="truncate text-[18px] leading-6 font-semibold text-[var(--text)]" title={data.label}>{data.label}</span>
      <span className="mt-2.5 inline-flex min-w-0 items-center gap-2.5 text-[13px] font-medium text-[var(--muted)]" title={data.enabled ? data.action : "off"}>
        <span className={cn("size-2.5 shrink-0 rounded-full", accentClass)} aria-hidden="true" />
        <span className="truncate">{data.enabled ? data.action : "off"}</span>
      </span>
      <Handle
        type="source"
        position={output}
        className="!size-1 !border-0 !bg-transparent !opacity-0"
      />
    </div>
  );
}

const pipelineNodeTypes = { pipeline: PipelineGraphNode };

function EffectivePolicyFlow({
  order,
  policyNodes,
  sourceNode,
  blockedVersionsNode,
  approvalNode,
  repositoryType,
  groupMemberCount,
}: {
  order: PolicyName[];
  policyNodes: Record<PolicyName, PolicyDisplayNode>;
  sourceNode: PolicyDisplayNode;
  blockedVersionsNode: PolicyDisplayNode;
  approvalNode: PolicyDisplayNode;
  repositoryType: Repository["type"];
  groupMemberCount: number;
}) {
  const { t } = useTranslation();
  const graphContainerRef = useRef<HTMLDivElement>(null);
  const [horizontal, setHorizontal] = useState(() => typeof window !== "undefined" && window.innerWidth >= 960);
  const [flowInstance, setFlowInstance] = useState<ReactFlowInstance<PipelineGraphNodeType, FlowEdge> | null>(null);
  const colorMode = typeof document !== "undefined" && document.documentElement.classList.contains("dark") ? "dark" : "light";
  const proxyItems: Array<{
    id: string;
    node: PolicyDisplayNode;
    stage: string;
    marker: string;
    fixed?: boolean;
  }> = [
    { id: "access", node: sourceNode, stage: t("repo.policy-flow-stage-access"), marker: "ACL", fixed: true },
    { id: "blocked_versions", node: blockedVersionsNode, stage: t("repo.policy-flow-stage-guard"), marker: t("repo.policy-fixed"), fixed: true },
    ...order.map((name, index) => ({ id: name, node: policyNodes[name], stage: t("repo.policy-flow-stage-assessment"), marker: String(index + 1) })),
    { id: "approval", node: approvalNode, stage: t("repo.policy-flow-stage-decision"), marker: t("repo.policy-final"), fixed: true },
  ];
  const memberLookupNode: PolicyDisplayNode = {
    name: "Member lookup",
    label: t("repo.group-member-lookup"),
    enabled: true,
    severity: "audit",
    action: `${groupMemberCount} · ${t("repo.group-members-in-order")}`,
  };
  const memberControlsNode: PolicyDisplayNode = {
    name: "Member controls",
    label: t("repo.group-member-controls"),
    enabled: true,
    severity: "audit",
    action: t("repo.group-member-controls-action"),
  };
  const graphItems = repositoryType === "proxy" || repositoryType === "hosted"
    ? proxyItems
    : repositoryType === "group"
      ? [
          { id: "access", node: sourceNode, stage: t("repo.policy-flow-stage-access"), marker: "ACL", fixed: true },
          { id: "member_lookup", node: memberLookupNode, stage: t("repo.policy-flow-stage-routing"), marker: t("repo.policy-fixed"), fixed: true },
          { id: "member_controls", node: memberControlsNode, stage: t("repo.policy-flow-stage-member"), marker: t("repo.policy-fixed"), fixed: true },
        ]
      : [
          { id: "access", node: sourceNode, stage: t("repo.policy-flow-stage-access"), marker: "ACL", fixed: true },
        ];
  const spacing = horizontal ? 348 : 285;
  const fitPadding = horizontal ? 0.025 : 0.04;
  const orderKey = order.join("|");
  const verticalHeight = Math.max(360, graphItems.length * spacing + 84);
  const nodes: PipelineGraphNodeType[] = graphItems.map((item, index) => ({
    id: item.id,
    type: "pipeline",
    position: horizontal ? { x: index * spacing, y: 48 } : { x: 48, y: index * spacing },
    draggable: false,
    selectable: false,
    data: {
      label: item.node.label,
      stage: item.stage,
      marker: item.marker,
      enabled: item.node.enabled,
      severity: item.node.severity,
      action: item.node.action,
      vertical: !horizontal,
      fixed: item.fixed,
    },
  }));
  const edges: FlowEdge[] = graphItems.slice(0, -1).map((item, index) => {
    const nextItem = graphItems[index + 1];
    const active = item.node.enabled && nextItem.node.enabled;
    const edgeColor = active ? "var(--accent)" : "var(--fx-text-subtle)";
    return {
      id: `${item.id}-${nextItem.id}`,
      source: item.id,
      target: nextItem.id,
      type: "straight",
      animated: active,
      markerEnd: { type: MarkerType.ArrowClosed, width: 12, height: 12, color: edgeColor },
      style: {
        stroke: edgeColor,
        strokeWidth: 2,
        strokeLinecap: "round",
        opacity: active ? 1 : 0.45,
      },
    };
  });

  useEffect(() => {
    const container = graphContainerRef.current;
    if (!container) return;

    let frame = 0;
    const observer = new ResizeObserver(([entry]) => {
      const nextHorizontal = entry.contentRect.width >= 960;
      setHorizontal(nextHorizontal);
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(() => flowInstance?.fitView({ padding: nextHorizontal ? 0.025 : 0.04 }));
    });
    observer.observe(container);
    return () => {
      cancelAnimationFrame(frame);
      observer.disconnect();
    };
  }, [flowInstance]);

  useEffect(() => {
    if (!flowInstance) return;
    const frame = requestAnimationFrame(() => flowInstance.fitView({ padding: fitPadding }));
    return () => cancelAnimationFrame(frame);
  }, [flowInstance, fitPadding, horizontal, orderKey]);

  return (
    <div
      ref={graphContainerRef}
      className={cn(
        "min-w-0 overflow-hidden rounded-[var(--radius)] border border-border bg-input/20",
        horizontal && "h-[360px]",
      )}
      style={horizontal ? undefined : { height: verticalHeight }}
    >
      <ReactFlow
        key={horizontal ? "horizontal" : "vertical"}
        nodes={nodes}
        edges={edges}
        nodeTypes={pipelineNodeTypes}
        colorMode={colorMode}
        onInit={setFlowInstance}
        fitView
        fitViewOptions={{ padding: fitPadding }}
        maxZoom={1}
        nodesDraggable={false}
        nodesConnectable={false}
        elementsSelectable={false}
        panOnDrag={false}
        zoomOnScroll={false}
        zoomOnPinch={false}
        zoomOnDoubleClick={false}
        preventScrolling={false}
        proOptions={{ hideAttribution: true }}
        aria-label={t("repo.policy-effective-flow")}
      />
    </div>
  );
}

// The phase boundaries are fixed; only automated assessment filters can move.
export type PolicySelection = "access" | "blocked_versions" | PolicyName | "approval";

export function PolicyFlow({
  repo,
  setRepo,
  canWrite,
  view,
  accessOnly = false,
  selectedPolicy,
  onSelectPolicy,
}: {
  repo: Repository;
  setRepo: RepositoryDraftSetter;
  canWrite: boolean;
  view: "settings" | "flow";
  accessOnly?: boolean;
  selectedPolicy: PolicySelection;
  onSelectPolicy: (policy: PolicySelection) => void;
}) {
  const { t } = useTranslation();
  const config = repo.config;
  const ipacl = config.ip_acl;
  const approval = config.approval;
  const vuln = config.vuln;
  const license = config.license;
  const age = config.age_policy;
  const order = effectivePolicyOrder(repo);
  // Policies with no effect on this repository are hidden entirely so the
  // pipeline only shows what is fully usable: release-age metadata has no
  // meaning for hosted uploads, and the vulnerability/license gates no-op for
  // OCI (OSV and deps.dev have no container-image ecosystem).
  const hiddenPolicies: PolicyName[] = [
    ...(repo.type === "hosted" ? (["age"] as PolicyName[]) : []),
    ...(repo.format === "oci" ? (["vulnerability", "license"] as PolicyName[]) : []),
  ];
  const displayOrder = order.filter((name) => !hiddenPolicies.includes(name));
  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 6 } }),
    useSensor(KeyboardSensor, { coordinateGetter: sortableKeyboardCoordinates }),
  );

  const sev = (action?: string): PolicySeverity =>
    action === "block" ? "block" : action === "warn" ? "warn" : "audit";

  const policyNodes: Record<PolicyName, PolicyDisplayNode> = {
    vulnerability: {
      name: "Vulnerability",
      label: t("repo.vuln-policy"),
      enabled: !!vuln?.enabled,
      severity: sev(vuln?.action),
      action: `${vuln?.action || "audit"} · ≥${vuln?.threshold || "high"}`,
      policy: "vulnerability",
    },
    license: {
      name: "License",
      label: t("common.license"),
      enabled: !!license?.enabled,
      severity: sev(license?.action),
      action: license?.action || "audit",
      policy: "license",
    },
    age: {
      name: "Age",
      label: t("repo.age-policy"),
      enabled: !!age?.enabled,
      severity: age?.action === "warn" ? "warn" : "block",
      action: `${age?.action || "block"}${age?.min_age && age.min_age !== "0s" ? ` · <${age.min_age}` : ""}`,
      policy: "age",
    },
  };
  const sourceNode: PolicyDisplayNode = {
    name: "Source IP ACL",
    label: t("repo.source-ip-acl"),
    enabled: !!ipacl?.enabled,
    severity: "block",
    action: `allow-list · ${(ipacl?.allow ?? []).length}`,
  };
  const blockedVersionsNode: PolicyDisplayNode = {
    name: "Blocked versions",
    label: t("approval.version-denies"),
    enabled: true,
    severity: "block",
    action: t("repo.policy-always-enforce"),
  };
  const approvalNode: PolicyDisplayNode = {
    name: "Package approval",
    label: t("repo.package-approval"),
    enabled: !!approval?.enabled,
    severity: (approval?.mode || "enforce") === "enforce" ? "block" : "audit",
    action: (approval?.mode || "enforce") === "enforce" ? "enforce" : "audit",
  };

  const setPolicyOrder = (next: PolicyName[]) => {
    // Hidden policies stay in the persisted order (the server validates that
    // every step is present); they re-append in their original relative order.
    const persisted = [...next, ...order.filter((name) => hiddenPolicies.includes(name))];
    setRepo({
      ...repo,
      config: {
        ...repo.config,
        policy_pipeline: { schema_version: 2, order: persisted },
      },
    });
  };

  const setPolicyEnabled = (policy: PolicyName, enabled: boolean) => {
    const nextConfig = { ...repo.config };
    if (policy === "vulnerability") {
      nextConfig.vuln = { ...(vuln ?? { action: "audit", threshold: "high", ignore: [] }), enabled };
    } else if (policy === "license") {
      nextConfig.license = { ...(license ?? { action: "audit", deny: [], allow: [] }), enabled };
    } else {
      nextConfig.age_policy = { ...(age ?? { min_age: "0s", action: "block" }), enabled };
    }
    setRepo({ ...repo, config: nextConfig });
  };

  const handleDragEnd = ({ active, over }: DragEndEvent) => {
    if (!over || active.id === over.id) return;
    const from = displayOrder.indexOf(active.id as PolicyName);
    const to = displayOrder.indexOf(over.id as PolicyName);
    if (from < 0 || to < 0) return;
    setPolicyOrder(arrayMove(displayOrder, from, to));
  };

  const editorAccentClass = (n: PolicyDisplayNode) => n.enabled
    ? n.severity === "block"
      ? "bg-[var(--danger)]"
      : n.severity === "warn"
        ? "bg-[var(--accent)]"
        : "bg-[var(--fx-info)]"
    : "bg-[var(--border-strong)]";

  const editorActionClass = (n: PolicyDisplayNode) => n.enabled
    ? n.severity === "block"
      ? "text-[var(--danger)]"
      : n.severity === "warn"
        ? "text-[var(--accent)]"
        : "text-[var(--fx-info)]"
    : "text-[var(--muted)]";

  const policyRowClass = (n: PolicyDisplayNode) => cn(
    "flex min-h-[72px] w-full items-center gap-2 overflow-hidden rounded-[var(--radius)] border border-[var(--border)] bg-[var(--panel-2)] px-2.5 py-2 shadow-[var(--fx-panel-highlight)] transition-[background-color,border-color,box-shadow,transform]",
    n.enabled && "border-[var(--border-strong)]",
  );

  const nodeContent = (n: PolicyDisplayNode, marker: ReactNode, onSelect: () => void) => (
    <>
      <span className={cn("absolute top-2 bottom-2 left-0 w-0.5 rounded-r-full", editorAccentClass(n))} aria-hidden="true" />
      <button type="button" onClick={onSelect} aria-pressed={selectedPolicy === n.policy} className="flex min-w-0 flex-1 items-center gap-2 rounded-[var(--radius)] text-left outline-none focus-visible:ring-2 focus-visible:ring-accent-ink">
        <span className="inline-flex size-6 shrink-0 items-center justify-center rounded-full border border-[var(--border-strong)] text-[10px] font-bold text-[var(--text)] tabular-nums">
          {marker}
        </span>
        <span className="min-w-0 flex-1">
          <span className="block truncate text-[13px] leading-5 font-semibold text-[var(--text)]" title={n.label}>{n.label}</span>
          <span
            className={cn("flex min-w-0 items-center gap-1.5 truncate text-[10px] font-medium", editorActionClass(n))}
            title={n.enabled ? n.action : "off"}
          >
            <span className={cn("size-1.5 shrink-0 rounded-full", editorAccentClass(n))} aria-hidden="true" />
            <span className="truncate">{n.enabled ? n.action : "off"}</span>
          </span>
        </span>
      </button>
      <div
        className="flex shrink-0 items-center gap-2"
        onPointerDown={(event) => event.stopPropagation()}
        onKeyDown={(event) => event.stopPropagation()}
        onClick={(event) => event.stopPropagation()}
      >
        <Switch
          size="sm"
          checked={n.enabled}
          disabled={!canWrite}
          onCheckedChange={(enabled) => setPolicyEnabled(n.policy!, enabled)}
          aria-label={`${n.label}: ${n.enabled ? t("repo.setting-enabled") : t("repo.setting-disabled")}`}
        />
        <span className="truncate text-[10px] font-medium text-[var(--muted)]">
          {n.enabled ? t("repo.setting-enabled") : t("repo.setting-disabled")}
        </span>
      </div>
    </>
  );

  const fixedNodeRow = (
    selection: PolicySelection,
    node: PolicyDisplayNode,
    marker: ReactNode,
    control?: ReactNode,
  ) => (
    <div className={cn(
      "relative flex min-h-[72px] items-center gap-2 overflow-hidden rounded-[var(--radius)] border border-[var(--border)] bg-[var(--panel-2)] px-2.5 py-2 transition-[background-color,border-color]",
      node.enabled && "border-[var(--border-strong)]",
      selectedPolicy === selection && "bg-[var(--panel-3)] ring-1 ring-[var(--border-strong)]",
    )}>
      <span className={cn("absolute top-2 bottom-2 left-0 w-0.5 rounded-r-full", editorAccentClass(node))} aria-hidden="true" />
      <span className="flex size-8 shrink-0 items-center justify-center text-muted-foreground">
        <LockKeyhole className="size-4" aria-hidden="true" />
      </span>
      <button
        type="button"
        onClick={() => onSelectPolicy(selection)}
        aria-pressed={selectedPolicy === selection}
        className="flex min-w-0 flex-1 items-center gap-2 rounded-[var(--radius)] text-left outline-none focus-visible:ring-2 focus-visible:ring-accent-ink"
      >
        <span className="inline-flex min-w-6 shrink-0 items-center justify-center rounded-full border border-[var(--border-strong)] px-1.5 text-[9px] font-bold text-[var(--text)]">
          {marker}
        </span>
        <span className="min-w-0 flex-1">
          <span className="block truncate text-[13px] font-semibold" title={node.label}>{node.label}</span>
          <span className={cn("flex min-w-0 items-center gap-1.5 text-[10px] font-medium", editorActionClass(node))}>
            <span className={cn("size-1.5 shrink-0 rounded-full", editorAccentClass(node))} aria-hidden="true" />
            <span className="truncate">{node.enabled ? node.action : "off"}</span>
          </span>
        </span>
      </button>
      {control && <div className="flex shrink-0 items-center gap-2">{control}</div>}
    </div>
  );

  const phaseLabel = (label: string) => (
    <div className="mb-1.5 mt-4 text-[10px] font-semibold uppercase text-muted-foreground first:mt-0">{label}</div>
  );

  return (
    <Card size="sm" className="mb-0 rounded-none border-0 bg-transparent py-0 shadow-none ring-0">
      <CardContent className="px-0 py-0">
        {view === "settings" ? (
        <section
          aria-label={accessOnly ? t("repo.access-control") : t("repo.policy-order")}
        >
          <div className="min-w-0 max-w-full">
            {phaseLabel(t("repo.policy-stage-access"))}
            {fixedNodeRow("access", sourceNode, "ACL", (
              <>
                <Switch
                  size="sm"
                  checked={sourceNode.enabled}
                  disabled={!canWrite}
                  onCheckedChange={(enabled) => setRepo({ ...repo, config: { ...repo.config, ip_acl: { ...(ipacl ?? { allow: [] }), enabled } } })}
                  aria-label={`${sourceNode.label}: ${sourceNode.enabled ? t("repo.setting-enabled") : t("repo.setting-disabled")}`}
                />
                <span className="hidden text-[10px] font-medium text-muted-foreground sm:inline">{sourceNode.enabled ? t("repo.setting-enabled") : t("repo.setting-disabled")}</span>
              </>
            ))}

            {!accessOnly && (
              <>
                {phaseLabel(t("repo.policy-stage-artifact-guard"))}
                {fixedNodeRow("blocked_versions", blockedVersionsNode, t("repo.policy-fixed"))}

                {displayOrder.length > 0 && phaseLabel(t("repo.policy-stage-automated"))}
                <DndContext sensors={sensors} collisionDetection={closestCenter} onDragEnd={handleDragEnd}>
                  <SortableContext items={displayOrder} strategy={verticalListSortingStrategy}>
                    <div className="flex min-w-0 flex-col gap-2">
                      {displayOrder.map((name, i) => {
                        const n = policyNodes[name];
                        return (
                          <SortablePolicyRow
                            key={name}
                            policy={name}
                            canWrite={canWrite}
                            dragLabel={`${t("repo.policy-drag")}: ${n.label}`}
                            className={policyRowClass(n)}
                            selected={selectedPolicy === name}
                          >
                            {nodeContent(n, i + 1, () => onSelectPolicy(name))}
                          </SortablePolicyRow>
                        );
                      })}
                    </div>
                  </SortableContext>
                </DndContext>
              </>
            )}
          </div>
          {!accessOnly && <>
            {phaseLabel(t("repo.policy-stage-approval"))}
            <div
            className={cn(
              "relative flex min-h-[72px] items-center gap-2 overflow-hidden rounded-[var(--radius)] border border-[var(--border)] bg-[var(--panel-2)] px-2.5 py-2 outline-none transition-[background-color,border-color] focus-visible:ring-2 focus-visible:ring-accent-ink",
              selectedPolicy === "approval" && "border-[var(--border-strong)] bg-[var(--panel-3)]",
            )}
          >
            <span className="flex size-8 shrink-0 items-center justify-center text-muted-foreground"><LockKeyhole className="size-4" aria-hidden="true" /></span>
            <button type="button" onClick={() => onSelectPolicy("approval")} aria-pressed={selectedPolicy === "approval"} className="flex min-w-0 flex-1 items-center gap-2 rounded-[var(--radius)] text-left outline-none focus-visible:ring-2 focus-visible:ring-accent-ink">
              <span className="inline-flex size-6 shrink-0 items-center justify-center rounded-full border border-[var(--border-strong)] text-[10px] font-bold">{t("repo.policy-final")}</span>
              <span className="min-w-0 flex-1">
                <span className="block truncate text-[13px] font-semibold">{approvalNode.label}</span>
                <span className={cn("flex items-center gap-1.5 text-[10px] font-medium", editorActionClass(approvalNode))}>
                  <span className={cn("size-1.5 rounded-full", editorAccentClass(approvalNode))} aria-hidden="true" />
                  {approvalNode.enabled ? approvalNode.action : "off"}
                </span>
              </span>
            </button>
            <div className="flex shrink-0 items-center gap-2" onPointerDown={(event) => event.stopPropagation()} onKeyDown={(event) => event.stopPropagation()} onClick={(event) => event.stopPropagation()}>
              <Switch
                size="sm"
                checked={approvalNode.enabled}
                disabled={!canWrite}
                onCheckedChange={(enabled) => setRepo({ ...repo, config: { ...repo.config, approval: { ...(approval ?? { mode: "enforce", auto_approve: [] }), enabled } } })}
                aria-label={`${approvalNode.label}: ${approvalNode.enabled ? t("repo.setting-enabled") : t("repo.setting-disabled")}`}
              />
              <span className="hidden text-[10px] font-medium text-muted-foreground sm:inline">{approvalNode.enabled ? t("repo.setting-enabled") : t("repo.setting-disabled")}</span>
            </div>
            </div>
          </>}
        </section>
        ) : (
        <section aria-labelledby="policy-effective-flow-heading">
          <h3 id="policy-effective-flow-heading" className="m-0 mb-2 text-sm font-semibold">{t("repo.policy-effective-flow")}</h3>
          <EffectivePolicyFlow
            order={displayOrder}
            policyNodes={policyNodes}
            sourceNode={sourceNode}
            blockedVersionsNode={blockedVersionsNode}
            approvalNode={approvalNode}
            repositoryType={repo.type}
            groupMemberCount={repo.config.group?.members?.length ?? 0}
          />
        </section>
        )}
      </CardContent>
    </Card>
  );
}
