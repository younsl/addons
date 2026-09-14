import React, { PropsWithChildren, useEffect, useState } from 'react';
import CategoryIcon from '@material-ui/icons/Category';
import ExtensionIcon from '@material-ui/icons/Extension';
import LibraryBooks from '@material-ui/icons/LibraryBooks';
import SearchIcon from '@material-ui/icons/Search';
import GroupIcon from '@material-ui/icons/Group';
import BuildIcon from '@material-ui/icons/Build';
import CloudUploadIcon from '@material-ui/icons/CloudUpload';
import ExpandMoreIcon from '@material-ui/icons/ExpandMore';
import ExpandLessIcon from '@material-ui/icons/ExpandLess';
import FavoriteBorderIcon from '@material-ui/icons/FavoriteBorder';
import SecurityIcon from '@material-ui/icons/Security';
import StorageIcon from '@material-ui/icons/Storage';
import AttachMoneyIcon from '@material-ui/icons/AttachMoney';
import FindInPageIcon from '@material-ui/icons/FindInPage';
import VpnKeyIcon from '@material-ui/icons/VpnKey';
import TrendingUpIcon from '@material-ui/icons/TrendingUp';
import VerifiedUserIcon from '@material-ui/icons/VerifiedUser';
import FingerprintIcon from '@material-ui/icons/Fingerprint';
import { Text } from '@backstage/ui';
import { siArgo, siGitlab, siKubernetes } from 'simple-icons';
import { createIcon } from '@dweber019/backstage-plugin-simple-icons';

const ArgocdIcon = createIcon(siArgo, false);
const GitlabIcon = createIcon(siGitlab, false);
const KubernetesIcon = createIcon(siKubernetes, false);
import {
  Settings as SidebarSettings,
  UserSettingsSignInAvatar,
} from '@backstage/plugin-user-settings';
import { SidebarSearchModal } from '@backstage/plugin-search';
import {
  Sidebar,
  sidebarConfig,
  SidebarDivider,
  SidebarGroup,
  SidebarItem,
  SidebarPage,
  SidebarScrollWrapper,
  SidebarSpace,
  useSidebarOpenState,
  Link,
} from '@backstage/core-components';
import { MyGroupsSidebarItem } from '@backstage/plugin-org';
import {
  configApiRef,
  discoveryApiRef,
  fetchApiRef,
  identityApiRef,
  useApi,
} from '@backstage/core-plugin-api';
import LogoFull from './LogoFull';
import LogoIcon from './LogoIcon';
import './Root.css';

const SidebarLogo = () => {
  const { isOpen } = useSidebarOpenState();

  return (
    // The height is Backstage's own logo metric, not a design choice here, so it
    // stays in JS rather than being duplicated as a magic number in the CSS.
    <div
      className="sidebar-logo"
      style={{ height: 3 * sidebarConfig.logoHeight }}
    >
      <Link to="/" underline="none" className="sidebar-logo-link" aria-label="Home">
        {isOpen ? <LogoFull /> : <LogoIcon />}
      </Link>
    </div>
  );
};

const CurrentUser = () => {
  const { isOpen } = useSidebarOpenState();
  const identityApi = useApi(identityApiRef);
  const [displayName, setDisplayName] = useState<string>('');

  useEffect(() => {
    identityApi.getProfileInfo().then(profile => {
      setDisplayName(profile.displayName || 'Guest');
    });
  }, [identityApi]);

  if (!isOpen) return null;

  return (
    <div className="sidebar-user">
      <Text variant="body-small" className="sidebar-user-name">
        Logged in as: {displayName}
      </Text>
    </div>
  );
};

interface FoldableSectionProps {
  title: string;
  icon: React.ReactElement;
  defaultOpen?: boolean;
  children: React.ReactNode;
}

const FoldableSection = ({
  title,
  icon,
  defaultOpen = true,
  children,
}: FoldableSectionProps) => {
  const { isOpen: sidebarOpen } = useSidebarOpenState();
  const [expanded, setExpanded] = useState(defaultOpen);

  const handleToggle = () => {
    setExpanded(!expanded);
  };

  if (!sidebarOpen) {
    return (
      <div
        className="sidebar-section-header sidebar-section-header-collapsed"
        onClick={handleToggle}
        role="button"
        tabIndex={0}
        onKeyDown={e => e.key === 'Enter' && handleToggle()}
      >
        {React.cloneElement(icon, { className: 'sidebar-section-icon' })}
      </div>
    );
  }

  return (
    <>
      <div
        className="sidebar-section-header"
        onClick={handleToggle}
        role="button"
        tabIndex={0}
        aria-expanded={expanded}
        onKeyDown={e => e.key === 'Enter' && handleToggle()}
      >
        {React.cloneElement(icon, { className: 'sidebar-section-icon' })}
        <Text variant="body-x-small" weight="bold" className="sidebar-section-title">
          {title}
        </Text>
        {expanded ? (
          <ExpandLessIcon className="sidebar-section-expand" />
        ) : (
          <ExpandMoreIcon className="sidebar-section-expand" />
        )}
      </div>
      {/*
        Grid rows animate the fold the way MUI's Collapse did, without measuring
        the content. visibility drops the collapsed items out of the tab order,
        which overflow alone would not do.
      */}
      <div
        className={`sidebar-section-panel${expanded ? ' sidebar-section-panel-open' : ''}`}
      >
        <div>{children}</div>
      </div>
    </>
  );
};

const IamAuditSidebarItem = () => {
  const discoveryApi = useApi(discoveryApiRef);
  const fetchApi = useApi(fetchApiRef);
  const [pendingCount, setPendingCount] = useState(0);

  useEffect(() => {
    const fetchPending = async () => {
      try {
        const baseUrl = await discoveryApi.getBaseUrl('iam-user-audit');
        const response = await fetchApi.fetch(
          `${baseUrl}/password-reset/requests`,
        );
        const data = await response.json();
        setPendingCount(
          data.filter((r: any) => r.status === 'pending').length,
        );
      } catch {
        /* ignore */
      }
    };
    fetchPending();
    const interval = setInterval(fetchPending, 60_000);
    return () => clearInterval(interval);
  }, [discoveryApi, fetchApi]);

  return (
    <SidebarItem icon={SecurityIcon} to="iam-user-audit" text="IAM Audit">
      <span
        className={
          pendingCount > 0 ? 'sidebar-badge' : 'sidebar-badge sidebar-badge-zero'
        }
      >
        {pendingCount}
      </span>
    </SidebarItem>
  );
};

const S3LogExtractSidebarItem = () => {
  const discoveryApi = useApi(discoveryApiRef);
  const fetchApi = useApi(fetchApiRef);
  const [pendingCount, setPendingCount] = useState(0);

  useEffect(() => {
    const fetchPending = async () => {
      try {
        const baseUrl = await discoveryApi.getBaseUrl('s3-log-extract');
        const response = await fetchApi.fetch(`${baseUrl}/requests`);
        const data = await response.json();
        setPendingCount(
          data.filter((r: any) => r.status === 'pending').length,
        );
      } catch {
        /* ignore */
      }
    };
    fetchPending();
    const interval = setInterval(fetchPending, 15_000);
    return () => clearInterval(interval);
  }, [discoveryApi, fetchApi]);

  return (
    <SidebarItem icon={FindInPageIcon} to="s3-log-extract" text="S3 Log Extract">
      <span
        className={
          pendingCount > 0 ? 'sidebar-badge' : 'sidebar-badge sidebar-badge-zero'
        }
      >
        {pendingCount}
      </span>
    </SidebarItem>
  );
};

const GitlabTokenAuditSidebarItem = () => {
  const discoveryApi = useApi(discoveryApiRef);
  const fetchApi = useApi(fetchApiRef);
  const [visible, setVisible] = useState(false);
  const [expiringCount, setExpiringCount] = useState(0);

  useEffect(() => {
    let cancelled = false;
    const fetchData = async () => {
      try {
        const baseUrl = await discoveryApi.getBaseUrl('gitlab-token-audit');
        const adminRes = await fetchApi.fetch(`${baseUrl}/admin-status`);
        if (!adminRes.ok) return;
        const adminData = await adminRes.json();
        if (!adminData?.isAdmin) {
          if (!cancelled) setVisible(false);
          return;
        }
        if (!cancelled) setVisible(true);
        const statusRes = await fetchApi.fetch(`${baseUrl}/status`);
        if (!statusRes.ok) return;
        const status = await statusRes.json();
        if (!cancelled) {
          setExpiringCount(
            (status.expiringSoonTokens ?? 0) + (status.expiredTokens ?? 0),
          );
        }
      } catch {
        /* ignore */
      }
    };
    fetchData();
    const interval = setInterval(fetchData, 60_000);
    return () => {
      cancelled = true;
      clearInterval(interval);
    };
  }, [discoveryApi, fetchApi]);

  if (!visible) return null;

  return (
    <SidebarItem icon={GitlabIcon} to="gitlab-token-audit" text="GitLab Tokens">
      <span
        className={
          expiringCount > 0 ? 'sidebar-badge' : 'sidebar-badge sidebar-badge-zero'
        }
      >
        {expiringCount}
      </span>
    </SidebarItem>
  );
};

/**
 * Admin-only section. Visibility follows the backend's admin check rather
 * than a frontend list so the sidebar and the page agree on who is an admin.
 * The badge is the count of denied token calls in the last 24h; the audit
 * log itself is a tab inside the Access Tokens page.
 */
const PatSidebarSection = () => {
  const discoveryApi = useApi(discoveryApiRef);
  const fetchApi = useApi(fetchApiRef);
  const [visible, setVisible] = useState(false);
  const [deniedCount, setDeniedCount] = useState(0);

  useEffect(() => {
    let cancelled = false;
    const fetchData = async () => {
      try {
        const baseUrl = await discoveryApi.getBaseUrl('pat');
        const adminRes = await fetchApi.fetch(`${baseUrl}/admin-status`);
        if (!adminRes.ok) return;
        const adminData = await adminRes.json();
        if (!adminData?.isAdmin) {
          if (!cancelled) setVisible(false);
          return;
        }
        if (!cancelled) setVisible(true);
        const summaryRes = await fetchApi.fetch(`${baseUrl}/audit/summary`);
        if (!summaryRes.ok) return;
        const summary = await summaryRes.json();
        if (!cancelled) setDeniedCount(summary.denied ?? 0);
      } catch {
        /* ignore */
      }
    };
    fetchData();
    const interval = setInterval(fetchData, 60_000);
    return () => {
      cancelled = true;
      clearInterval(interval);
    };
  }, [discoveryApi, fetchApi]);

  if (!visible) return null;

  return (
    <FoldableSection title="Administration" icon={<VerifiedUserIcon />} defaultOpen={false}>
      <SidebarItem icon={FingerprintIcon} to="pat" text="Access Tokens">
        <span
          className={
            deniedCount > 0 ? 'sidebar-badge' : 'sidebar-badge sidebar-badge-zero'
          }
        >
          {deniedCount}
        </span>
      </SidebarItem>
    </FoldableSection>
  );
};

const PlatformsSidebarItem = () => {
  const configApi = useApi(configApiRef);
  const platformsCount = (configApi.getOptionalConfigArray('app.platforms') ?? []).length;

  return (
    <SidebarItem icon={KubernetesIcon} to="platforms" text="Platforms">
      <span
        className={
          platformsCount > 0 ? 'sidebar-badge' : 'sidebar-badge sidebar-badge-zero'
        }
      >
        {platformsCount}
      </span>
    </SidebarItem>
  );
};

export const Root = ({ children }: PropsWithChildren<{}>) => {
  const config = useApi(configApiRef);
  const catalogHealthEnabled = config.getOptionalBoolean('app.plugins.catalogHealth') ?? true;
  const argocdAppSetEnabled = config.getOptionalBoolean('app.plugins.argocdAppSet') ?? true;
  const iamUserAuditEnabled = config.getOptionalBoolean('app.plugins.iamUserAudit') ?? true;
  const s3LogExtractEnabled = config.getOptionalBoolean('app.plugins.s3LogExtract') ?? true;
  const opencostEnabled = config.getOptionalBoolean('app.plugins.opencost') ?? true;
  const gitlabTokenAuditEnabled = config.getOptionalBoolean('app.plugins.gitlabTokenAudit') ?? true;
  const opensearchAccountEnabled = config.getOptionalBoolean('app.plugins.opensearchAccount') ?? true;
  const opensearchScalingEnabled = config.getOptionalBoolean('app.plugins.opensearchScaling') ?? true;
  const patEnabled = config.getOptionalBoolean('app.plugins.pat') ?? true;

  return (
  <SidebarPage>
    <Sidebar>
      <SidebarLogo />
      <SidebarGroup label="Search" icon={<SearchIcon />} to="/search">
        <SidebarSearchModal />
      </SidebarGroup>
      <SidebarDivider />

      <FoldableSection title="Resources" icon={<CategoryIcon />} defaultOpen={false}>
        <PlatformsSidebarItem />
        <SidebarItem icon={CategoryIcon} to="catalog" text="Catalog" />
        <SidebarItem icon={ExtensionIcon} to="api-docs" text="APIs" />
        <SidebarItem icon={CloudUploadIcon} to="openapi-registry" text="API Registry" />
        <SidebarItem icon={LibraryBooks} to="docs" text="Docs" />
      </FoldableSection>

      <FoldableSection title="Operations" icon={<BuildIcon />} defaultOpen={false}>
        {catalogHealthEnabled && (
          <SidebarItem icon={FavoriteBorderIcon} to="catalog-health" text="Catalog Health" />
        )}
        {argocdAppSetEnabled && (
          <SidebarItem icon={ArgocdIcon} to="argocd-appset" text="ArgoCD" />
        )}
        {gitlabTokenAuditEnabled && <GitlabTokenAuditSidebarItem />}
        {opencostEnabled && (
          <SidebarItem icon={AttachMoneyIcon} to="cost-report" text="Cost Report" />
        )}
        {iamUserAuditEnabled && <IamAuditSidebarItem />}
        {opensearchAccountEnabled && (
          <SidebarItem icon={VpnKeyIcon} to="opensearch" text="OpenSearch" />
        )}
        {opensearchScalingEnabled && (
          <SidebarItem icon={TrendingUpIcon} to="opensearch-scaling" text="Capacity" />
        )}
        {s3LogExtractEnabled && <S3LogExtractSidebarItem />}
      </FoldableSection>

      {patEnabled && <PatSidebarSection />}

      <SidebarDivider />
      <SidebarScrollWrapper>
        <MyGroupsSidebarItem
          singularTitle="My Group"
          pluralTitle="My Groups"
          icon={GroupIcon}
        />
      </SidebarScrollWrapper>

      <SidebarSpace />
      <SidebarDivider />
      <CurrentUser />
      <SidebarGroup
        label="Settings"
        icon={<UserSettingsSignInAvatar />}
        to="/settings"
      >
        <SidebarSettings />
      </SidebarGroup>
    </Sidebar>
    {children}
  </SidebarPage>
  );
};
