import { canReviewApprovals } from "@/utils/permissions";
import { useEffect, useRef, useState, type ReactNode } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, Outlet, useLocation } from "@tanstack/react-router";
import {
  Bell,
  BookOpen,
  Boxes,
  ClipboardCheck,
  HardDrive,
  KeyRound,
  LogOut,
  PanelLeftClose,
  PanelLeftOpen,
  Settings,
  SlidersHorizontal,
  Target,
  UserRound,
  UserRoundCog,
  UsersRound,
} from "lucide-react";
import { api } from "@/api";
import type { Me } from "@/services/v1/openapi-types";
import { AuthProvider } from "@/authContext";
import { GlobalSearch } from "@/components/app/global-search";
import { Redirect } from "@/components/app/redirect";
import { Logo } from "@/components/app/logo";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { openApiQueryOptions } from "@/query/v1/openapi-query-options";
import { TooltipProvider } from "@/components/ui/tooltip";
import { useTranslation } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { useUserPreferences, useUserPreferenceActions } from "@/stores/user-preferences";

export function AppShell() {
  const { t } = useTranslation();
  const location = useLocation();
  const queryClient = useQueryClient();
  const contentWidthMode = useUserPreferences((state) => state.contentWidthMode);
  const meQueryOptions = openApiQueryOptions.getMe();
  // The generated queryFn, not a replacement for it. Not being signed in is a
  // 401, which arrives here as an error rather than as data - so the fallback
  // lives on this side of the query instead of inside a hand-written queryFn
  // that swallowed it. retry is off because a 401 will not become a 200.
  const { data, isLoading } = useQuery({
    ...meQueryOptions,
    retry: false,
    meta: { suppressGlobalErrorToast: true },
  });
  const me: Me = data ?? { authenticated: false };

  const refresh = () => queryClient.invalidateQueries({ queryKey: meQueryOptions.queryKey });

  if (isLoading) return <div className="flex min-h-screen w-full items-center justify-center">{t("common.loading")}</div>;

  if (!me.authenticated) {
    return location.pathname === "/login" ? <Outlet /> : <Redirect to="/login" replace />;
  }

  if (location.pathname === "/login") {
    return <Redirect to="/workspace/repositories" replace />;
  }

  return (
    <AuthProvider value={{ me }}>
      <TooltipProvider>
        {me.impersonator && <ImpersonationBanner me={me} />}
        <div className="min-h-[calc(100dvh-var(--fx-impersonation-h,0px))] bg-background text-foreground lg:flex lg:items-start">
          {/* Logging out only ends the session and re-reads it. Where to go next
              is not the button's decision: the unauthenticated branch above
              redirects to /login, and it is the single place that knows. */}
          <Sidebar me={me} onLogout={() => api.logout().then(refresh)} />
          <main className="w-full min-w-0 flex-1 px-[var(--fx-main-gutter-x)] py-[var(--fx-main-gutter-y)] max-lg:px-3 max-lg:py-3 max-sm:px-2 max-sm:py-2">
            <div className="min-h-[calc(100dvh-var(--fx-impersonation-h,0px)-(var(--fx-main-gutter-y)*2))] rounded-[var(--fx-radius-2xl)] border border-[var(--fx-border-subtle)] bg-[var(--fx-surface-panel)] shadow-[var(--fx-panel-highlight)] max-lg:min-h-[calc(100dvh-var(--fx-impersonation-h,0px)-88px)] max-sm:rounded-[var(--fx-radius-lg)]">
              <div
                className={cn(
                  "mx-auto w-full py-[var(--fx-page-y)] transition-[max-width,padding] duration-150 max-sm:px-4 max-sm:py-5",
                  contentWidthMode === "wide"
                    ? "max-w-none px-[var(--fx-page-x-wide)]"
                    : "max-w-[var(--fx-content-max)] px-[var(--fx-page-x)]"
                )}
              >
                <Outlet />
              </div>
            </div>
          </main>
        </div>
      </TooltipProvider>
    </AuthProvider>
  );
}

// ImpersonationBanner stays pinned above every page while an administrator is
// acting as another user, so the identity in effect is never in doubt. Ending it
// reloads the app: the session cookie changes underneath, and every cached query
// belongs to the impersonated identity.
function ImpersonationBanner({ me }: { me: Me }) {
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false);
  const stop = () => {
    setBusy(true);
    api.stopImpersonation()
      .then(() => window.location.assign("/access/users"))
      // The administrator's own account is gone or disabled; the server cleared
      // the cookie, so the only way forward is a fresh sign-in.
      .catch(() => window.location.assign("/login"));
  };
  // The sidebar sticks to the top of the viewport as well, so it has to start
  // below the banner instead of sliding under it. The height is published as a
  // CSS variable and re-measured on resize, because the banner wraps to two
  // lines on narrow screens and a hardcoded offset would be wrong there.
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const publish = () =>
      document.documentElement.style.setProperty("--fx-impersonation-h", `${el.offsetHeight}px`);
    publish();
    const observer = new ResizeObserver(publish);
    observer.observe(el);
    return () => {
      observer.disconnect();
      document.documentElement.style.removeProperty("--fx-impersonation-h");
    };
  }, []);

  // The bar is fully opaque: the page scrolls underneath it, and a warning that
  // page content shows through reads as decoration rather than as state.
  return (
    <div ref={ref}
      className="sticky top-0 z-50 flex flex-wrap items-center justify-center gap-x-3 gap-y-1.5 border-b border-[var(--fx-danger-hover)] bg-destructive px-4 py-2 text-sm text-destructive-foreground">
      <span className="inline-flex items-center gap-1.5">
        <UserRoundCog className="size-4" aria-hidden="true" />
        <span>
          {t("impersonate.banner")} <strong className="font-mono">{me.username}</strong>
          <span> ({t("impersonate.banner-as")} {me.impersonator})</span>
        </span>
      </span>
      <Button variant="outline" size="sm" type="button" disabled={busy} onClick={stop}>
        {busy ? t("impersonate.stopping") : t("impersonate.stop")}
      </Button>
    </div>
  );
}

function Sidebar({ me, onLogout }: { me: Me; onLogout: () => void }) {
  const { t } = useTranslation();
  const canApprove = canReviewApprovals(me);
  // Desktop-only collapse (Backstage-style icon rail); the mobile layout keeps
  // its horizontal bar, so every collapsed override is lg-scoped.
  const collapsed = useUserPreferences((state) => state.isSidebarCollapsed);
  const { toggleSidebar } = useUserPreferenceActions();
  const { data: repoCount = null } = useQuery({
    ...openApiQueryOptions.listRepositories(),
    select: (repositories) => repositories.length,
  });
  const { data: pendingCount = null } = useQuery({
    ...openApiQueryOptions.getApprovalsCount({ query: { status: "pending" } }),
    select: (result) => result.count,
    enabled: canApprove,
  });
  // The headline percentage rides in the sidebar badge, so the migration's
  // progress is visible from every page rather than only on its own.
  const { data: coveragePercent = null } = useQuery({
    ...openApiQueryOptions.getCoverage(),
    select: (overview) => overview.percent,
    // Coverage is unconfigured on most deployments, where this 503s; a retrying
    // query in the shell would then fire on every page.
    retry: false,
    staleTime: 60_000,
    meta: { suppressGlobalErrorToast: true },
  });
  // The build stamp does not change while the tab is open, and its absence is
  // cosmetic - the sidebar simply omits the line.
  const { data: version = null } = useQuery({
    ...openApiQueryOptions.getVersion(),
    retry: false,
    staleTime: Infinity,
    meta: { suppressGlobalErrorToast: true },
  });

  const navLinkClass = (active = false) =>
    cn(
      "group flex shrink-0 items-center gap-2 rounded-md border px-2 py-1.5 text-[13px] leading-5 transition-colors hover:no-underline max-sm:px-2",
      collapsed && "lg:justify-center lg:gap-0",
      active
        ? "border-transparent bg-[var(--fx-surface-selected)] text-foreground"
        : "border-transparent text-muted-foreground hover:bg-[var(--fx-surface-hover)] hover:text-foreground"
    );
  // In the collapsed rail only the icon stays visible on desktop; labels and
  // badges keep rendering for the mobile bar.
  const labelClass = cn(collapsed && "lg:hidden");

  return (
    <aside
      className={cn(
        // --fx-impersonation-h is set only while the impersonation banner is up;
        // it keeps the sidebar docked below the banner instead of under it.
        "sticky top-[var(--fx-impersonation-h,0px)] z-40 flex h-[calc(100dvh-var(--fx-impersonation-h,0px))] shrink-0 flex-col gap-1 overflow-y-auto border-r border-[var(--fx-border-subtle)] bg-[var(--fx-sidebar-bg)] py-4 transition-[width] duration-150 lg:self-start max-lg:h-auto max-lg:w-full max-lg:overflow-visible max-lg:border-r-0 max-lg:border-b max-lg:px-3 max-lg:py-2 max-sm:px-2",
        collapsed ? "w-16 px-2" : "w-[var(--fx-sidebar-width)] px-3"
      )}
    >
      <div className={cn("px-1 pb-3 max-lg:flex max-lg:items-center max-lg:justify-between max-lg:gap-3 max-lg:pb-2 max-sm:px-1", collapsed && "lg:px-0")}>
        <div className={cn("flex items-center justify-between gap-2", collapsed && "lg:flex-col lg:gap-2")}>
          <Link
            to="/workspace/repositories"
            className="flex min-w-0 items-center gap-2 text-sm font-medium text-foreground hover:no-underline hover:opacity-85"
            title="forklift"
          >
            <Logo />
            <span className={cn("truncate", labelClass)}>fork<span className="text-accent-ink">lift</span></span>
          </Link>
          {/* Icon-only collapse toggle, pinned top-right of the sidebar (below
              the logo in the collapsed rail). Desktop only: the mobile bar has
              nothing to collapse. */}
          <button
            type="button"
            onClick={toggleSidebar}
            title={collapsed ? t("nav.expand") : t("nav.collapse")}
            aria-label={collapsed ? t("nav.expand") : t("nav.collapse")}
            aria-expanded={!collapsed}
            className="flex size-7 shrink-0 cursor-pointer items-center justify-center rounded-md border border-transparent text-muted-foreground transition-colors hover:bg-[var(--fx-surface-hover)] hover:text-foreground max-lg:hidden"
          >
            {collapsed
              ? <PanelLeftOpen className="size-4" aria-hidden="true" />
              : <PanelLeftClose className="size-4" aria-hidden="true" />}
          </button>
        </div>
        {version && (
          <span className={cn("ml-9 mt-1 block shrink-0 text-[11px] font-medium text-[var(--fx-text-subtle)] max-lg:m-0 max-sm:hidden", labelClass)}>
            {version.version}
            {version.commit && version.commit !== "none" && (
              <span className="opacity-65"> ({version.commit.slice(0, 7)})</span>
            )}
          </span>
        )}
      </div>
      <div className={labelClass}>
        <GlobalSearch />
      </div>
      <nav className="-mx-1 flex flex-col gap-0.5 px-1 max-lg:flex-row max-lg:overflow-x-auto max-lg:pb-1 max-lg:[scrollbar-width:none] max-lg:[&::-webkit-scrollbar]:hidden">
        <NavGroup title={t("nav.group.workspace")} collapsed={collapsed}>
          <Link className={navLinkClass()} activeProps={{ className: navLinkClass(true) }} to="/workspace/repositories" title={collapsed ? t("nav.repositories") : undefined}>
            <Boxes className="size-4 opacity-75 group-hover:opacity-100" aria-hidden="true" />
            <span className={labelClass}>{t("nav.repositories")}</span>
            {repoCount !== null && <Badge data-testid="value-nav-repository-count" variant="outline" className={cn("ml-2 min-w-5 justify-center px-1.5 lg:ml-auto", labelClass)}>{repoCount}</Badge>}
          </Link>
          {canApprove && (
            <Link className={navLinkClass()} activeProps={{ className: navLinkClass(true) }} to="/workspace/approvals" title={collapsed ? t("nav.approvals") : undefined}>
              <ClipboardCheck className="size-4 opacity-75 group-hover:opacity-100" aria-hidden="true" />
              <span className={labelClass}>{t("nav.approvals")}</span>
              {pendingCount !== null && pendingCount > 0 && <Badge data-testid="value-nav-pending-count" className={cn("ml-2 min-w-5 justify-center bg-primary text-primary-foreground lg:ml-auto", labelClass)}>{pendingCount}</Badge>}
            </Link>
          )}
          {/* Coverage sits with the workspace rather than under Admin: it is the
              number the whole organisation is working towards, and hiding it
              behind admin would make it the number nobody can see. Running a
              scan and editing what is measured stay admin-only. */}
          <Link className={navLinkClass()} activeProps={{ className: navLinkClass(true) }} to="/workspace/coverage" title={collapsed ? t("nav.coverage") : undefined}>
            <Target className="size-4 opacity-75 group-hover:opacity-100" aria-hidden="true" />
            <span className={labelClass}>{t("nav.coverage")}</span>
            {coveragePercent !== null && <Badge data-testid="value-nav-coverage-percent" variant="outline" className={cn("ml-2 min-w-5 justify-center px-1.5 lg:ml-auto", labelClass)}>{coveragePercent}%</Badge>}
          </Link>
        </NavGroup>
        <NavGroup title={t("nav.group.access")} collapsed={collapsed}>
          <Link className={navLinkClass()} activeProps={{ className: navLinkClass(true) }} to="/workspace/tokens" title={collapsed ? t("nav.tokens") : undefined}>
            <KeyRound className="size-4 opacity-75 group-hover:opacity-100" aria-hidden="true" />
            <span className={labelClass}>{t("nav.tokens")}</span>
          </Link>
          {(me.admin || me.auditor) && (
            <>
              <Link className={navLinkClass()} activeProps={{ className: navLinkClass(true) }} to="/access/users" title={collapsed ? t("nav.users") : undefined}>
                <UserRound className="size-4 opacity-75 group-hover:opacity-100" aria-hidden="true" />
                <span className={labelClass}>{t("nav.users")}</span>
              </Link>
              <Link className={navLinkClass()} activeProps={{ className: navLinkClass(true) }} to="/access/roles" title={collapsed ? t("nav.roles") : undefined}>
                <UsersRound className="size-4 opacity-75 group-hover:opacity-100" aria-hidden="true" />
                <span className={labelClass}>{t("nav.roles")}</span>
              </Link>
            </>
          )}
        </NavGroup>
        <NavGroup title={t("nav.group.preferences")} collapsed={collapsed}>
          <Link className={navLinkClass()} activeProps={{ className: navLinkClass(true) }} to="/settings" title={collapsed ? t("nav.settings") : undefined}>
            <Settings className="size-4 opacity-75 group-hover:opacity-100" aria-hidden="true" />
            <span className={labelClass}>{t("nav.settings")}</span>
          </Link>
        </NavGroup>
        {me.admin && (
          <NavGroup title={t("nav.group.admin")} collapsed={collapsed}>
            <Link className={navLinkClass()} activeProps={{ className: navLinkClass(true) }} to="/admin/notifications" title={collapsed ? t("nav.notifications") : undefined}>
              <Bell className="size-4 opacity-75 group-hover:opacity-100" aria-hidden="true" />
              <span className={labelClass}>{t("nav.notifications")}</span>
            </Link>
            <Link className={navLinkClass()} activeProps={{ className: navLinkClass(true) }} to="/admin/storage" title={collapsed ? t("nav.storage") : undefined}>
              <HardDrive className="size-4 opacity-75 group-hover:opacity-100" aria-hidden="true" />
              <span className={labelClass}>{t("nav.storage")}</span>
            </Link>
            <Link className={navLinkClass()} activeProps={{ className: navLinkClass(true) }} to="/admin/ha" title={collapsed ? t("nav.ha") : undefined}>
              <SlidersHorizontal className="size-4 opacity-75 group-hover:opacity-100" aria-hidden="true" />
              <span className={labelClass}>{t("nav.ha")}</span>
            </Link>
          </NavGroup>
        )}
      </nav>
      <div className="flex-1" />
      <div className="mt-3 flex flex-col gap-1 border-t border-[var(--fx-border-subtle)] pt-3 max-lg:mt-2 max-lg:flex-row max-lg:items-center max-lg:justify-between max-lg:gap-3 max-lg:overflow-x-auto max-lg:pt-2 max-sm:gap-2">
        <a className={navLinkClass()} href="/api-docs" target="_blank" rel="noreferrer" title={collapsed ? t("nav.api-docs") : undefined}>
          <BookOpen className="size-4 opacity-75 group-hover:opacity-100" aria-hidden="true" />
          <span className={labelClass}>{t("nav.api-docs")}</span>
        </a>
        <div className={cn("min-w-0 px-2 text-xs text-muted-foreground max-lg:flex max-lg:items-center max-lg:gap-2 max-lg:px-0 max-sm:ml-auto max-sm:w-auto", collapsed && "lg:px-0")}>
          <div className={cn("truncate", labelClass)}>
            {me.username} {me.admin ? `(${t("role.admin")})` : me.auditor ? `(${t("role.auditor")})` : ""}
          </div>
          <Button
            className={cn("mt-2 w-full gap-1.5 max-lg:mt-0 max-lg:w-auto max-sm:shrink-0", collapsed && "lg:px-0")}
            variant="outline"
            type="button"
            title={collapsed ? t("nav.logout") : undefined}
            onClick={onLogout}
          >
            <LogOut className="size-3.5" aria-hidden="true" />
            <span className={labelClass}>{t("nav.logout")}</span>
          </Button>
        </div>
      </div>
    </aside>
  );
}

function NavGroup({ title, collapsed = false, children }: { title: ReactNode; collapsed?: boolean; children: ReactNode }) {
  return (
    <section className="contents lg:flex lg:flex-col lg:gap-0.5 lg:pt-3.5 first:lg:pt-0" aria-label={typeof title === "string" ? title : undefined}>
      <div className={cn("px-2 pb-1 text-[13px] font-medium leading-4 text-[var(--fx-text-subtle)] max-lg:hidden", collapsed && "lg:hidden")}>
        {title}
      </div>
      {children}
    </section>
  );
}
