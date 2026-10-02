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
import StorageIcon from '@material-ui/icons/Storage';
import AttachMoneyIcon from '@material-ui/icons/AttachMoney';
import { Text } from '@backstage/ui';
import { siArgo, siKubernetes } from 'simple-icons';
import { createIcon } from '@dweber019/backstage-plugin-simple-icons';

const ArgocdIcon = createIcon(siArgo, false);
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
  const argocdAppSetEnabled = config.getOptionalBoolean('app.plugins.argocdAppSet') ?? true;
  const opencostEnabled = config.getOptionalBoolean('app.plugins.opencost') ?? true;

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
        {argocdAppSetEnabled && (
          <SidebarItem icon={ArgocdIcon} to="argocd-appset" text="ArgoCD" />
        )}
        {opencostEnabled && (
          <SidebarItem icon={AttachMoneyIcon} to="cost-report" text="Cost Report" />
        )}
      </FoldableSection>

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
