import React from 'react';
import { Grid } from '@backstage/ui';
import {
  EntityApiDefinitionCard,
  EntityConsumedApisCard,
  EntityProvidedApisCard,
} from '@backstage/plugin-api-docs';
import {
  EntityAboutCard,
  EntityDependsOnComponentsCard,
  EntityDependsOnResourcesCard,
  EntityHasComponentsCard,
  EntityHasResourcesCard,
  EntityHasSubcomponentsCard,
  EntityHasSystemsCard,
  EntityLayout,
  EntityLinksCard,
  EntitySwitch,
  EntityOrphanWarning,
  EntityProcessingErrorsPanel,
  isComponentType,
  isKind,
  hasCatalogProcessingErrors,
  isOrphan,
} from '@backstage/plugin-catalog';
import {
  EntityUserProfileCard,
  EntityGroupProfileCard,
  EntityMembersListCard,
  EntityOwnershipCard,
} from '@backstage/plugin-org';
import { EntityCatalogGraphCard } from '@backstage/plugin-catalog-graph';
import {
  EntityTechdocsContent,
  isTechDocsAvailable,
} from '@backstage/plugin-techdocs';
const techdocsContent = (
  <EntityTechdocsContent>
    <Grid.Root columns="12" gap="3">
      <Grid.Item colSpan="12">
        <EntityAboutCard />
      </Grid.Item>
    </Grid.Root>
  </EntityTechdocsContent>
);

const entityWarningContent = (
  <>
    <EntitySwitch>
      <EntitySwitch.Case if={isOrphan}>
        <Grid.Item colSpan="12">
          <EntityOrphanWarning />
        </Grid.Item>
      </EntitySwitch.Case>
    </EntitySwitch>

    <EntitySwitch>
      <EntitySwitch.Case if={hasCatalogProcessingErrors}>
        <Grid.Item colSpan="12">
          <EntityProcessingErrorsPanel />
        </Grid.Item>
      </EntitySwitch.Case>
    </EntitySwitch>
  </>
);

const overviewContent = (
  <Grid.Root columns="12" gap="3">
    {entityWarningContent}
    <Grid.Item colSpan={{ initial: '12', md: '6' }}>
      <EntityAboutCard />
    </Grid.Item>
    <Grid.Item colSpan={{ initial: '12', md: '6' }}>
      <EntityCatalogGraphCard height={400} />
    </Grid.Item>
    <Grid.Item colSpan={{ initial: '12', md: '6' }}>
      <EntityLinksCard />
    </Grid.Item>
    <Grid.Item colSpan={{ initial: '12', md: '6' }}>
      <EntityHasSubcomponentsCard />
    </Grid.Item>
  </Grid.Root>
);

const serviceEntityPage = (
  <EntityLayout>
    <EntityLayout.Route path="/" title="Overview">
      {overviewContent}
    </EntityLayout.Route>

    <EntityLayout.Route path="/api" title="API">
      <Grid.Root columns="12" gap="3">
        <Grid.Item colSpan={{ initial: '12', md: '6' }}>
          <EntityProvidedApisCard />
        </Grid.Item>
        <Grid.Item colSpan={{ initial: '12', md: '6' }}>
          <EntityConsumedApisCard />
        </Grid.Item>
      </Grid.Root>
    </EntityLayout.Route>

    <EntityLayout.Route path="/dependencies" title="Dependencies">
      <Grid.Root columns="12" gap="3">
        <Grid.Item colSpan={{ initial: '12', md: '6' }}>
          <EntityDependsOnComponentsCard />
        </Grid.Item>
        <Grid.Item colSpan={{ initial: '12', md: '6' }}>
          <EntityDependsOnResourcesCard />
        </Grid.Item>
      </Grid.Root>
    </EntityLayout.Route>

    <EntityLayout.Route if={isTechDocsAvailable} path="/docs" title="Docs">
      {techdocsContent}
    </EntityLayout.Route>
  </EntityLayout>
);

const websiteEntityPage = (
  <EntityLayout>
    <EntityLayout.Route path="/" title="Overview">
      {overviewContent}
    </EntityLayout.Route>

    <EntityLayout.Route path="/dependencies" title="Dependencies">
      <Grid.Root columns="12" gap="3">
        <Grid.Item colSpan={{ initial: '12', md: '6' }}>
          <EntityDependsOnComponentsCard />
        </Grid.Item>
        <Grid.Item colSpan={{ initial: '12', md: '6' }}>
          <EntityDependsOnResourcesCard />
        </Grid.Item>
      </Grid.Root>
    </EntityLayout.Route>

    <EntityLayout.Route if={isTechDocsAvailable} path="/docs" title="Docs">
      {techdocsContent}
    </EntityLayout.Route>
  </EntityLayout>
);

const defaultEntityPage = (
  <EntityLayout>
    <EntityLayout.Route path="/" title="Overview">
      {overviewContent}
    </EntityLayout.Route>

    <EntityLayout.Route if={isTechDocsAvailable} path="/docs" title="Docs">
      {techdocsContent}
    </EntityLayout.Route>
  </EntityLayout>
);

const componentPage = (
  <EntitySwitch>
    <EntitySwitch.Case if={isComponentType('service')}>
      {serviceEntityPage}
    </EntitySwitch.Case>

    <EntitySwitch.Case if={isComponentType('website')}>
      {websiteEntityPage}
    </EntitySwitch.Case>

    <EntitySwitch.Case>{defaultEntityPage}</EntitySwitch.Case>
  </EntitySwitch>
);

const apiPage = (
  <EntityLayout>
    <EntityLayout.Route path="/" title="Overview">
      <Grid.Root columns="12" gap="3">
        {entityWarningContent}
        <Grid.Item colSpan={{ initial: '12', md: '6' }}>
          <EntityAboutCard />
        </Grid.Item>
        <Grid.Item colSpan={{ initial: '12', md: '6' }}>
          <EntityCatalogGraphCard height={400} />
        </Grid.Item>
        <Grid.Item colSpan={{ initial: '12', md: '6' }}>
          <EntityLinksCard />
        </Grid.Item>
      </Grid.Root>
    </EntityLayout.Route>

    <EntityLayout.Route path="/definition" title="Definition">
      <Grid.Root columns="12" gap="3">
        <Grid.Item colSpan="12">
          <EntityApiDefinitionCard />
        </Grid.Item>
      </Grid.Root>
    </EntityLayout.Route>
  </EntityLayout>
);

const userPage = (
  <EntityLayout>
    <EntityLayout.Route path="/" title="Overview">
      <Grid.Root columns="12" gap="3">
        {entityWarningContent}
        <Grid.Item colSpan={{ initial: '12', md: '6' }}>
          <EntityUserProfileCard />
        </Grid.Item>
        <Grid.Item colSpan={{ initial: '12', md: '6' }}>
          <EntityOwnershipCard />
        </Grid.Item>
      </Grid.Root>
    </EntityLayout.Route>
  </EntityLayout>
);

const groupPage = (
  <EntityLayout>
    <EntityLayout.Route path="/" title="Overview">
      <Grid.Root columns="12" gap="3">
        {entityWarningContent}
        <Grid.Item colSpan={{ initial: '12', md: '6' }}>
          <EntityGroupProfileCard />
        </Grid.Item>
        <Grid.Item colSpan={{ initial: '12', md: '6' }}>
          <EntityOwnershipCard />
        </Grid.Item>
        <Grid.Item colSpan="12">
          <EntityMembersListCard />
        </Grid.Item>
      </Grid.Root>
    </EntityLayout.Route>
  </EntityLayout>
);

const systemPage = (
  <EntityLayout>
    <EntityLayout.Route path="/" title="Overview">
      <Grid.Root columns="12" gap="3">
        {entityWarningContent}
        <Grid.Item colSpan={{ initial: '12', md: '6' }}>
          <EntityAboutCard />
        </Grid.Item>
        <Grid.Item colSpan={{ initial: '12', md: '6' }}>
          <EntityCatalogGraphCard height={400} />
        </Grid.Item>
        <Grid.Item colSpan={{ initial: '12', md: '6' }}>
          <EntityHasComponentsCard />
        </Grid.Item>
        <Grid.Item colSpan={{ initial: '12', md: '6' }}>
          <EntityHasResourcesCard />
        </Grid.Item>
      </Grid.Root>
    </EntityLayout.Route>
    <EntityLayout.Route path="/diagram" title="Diagram">
      <EntityCatalogGraphCard
               direction="TB"
        title="System Diagram"
        height={700}
      />
    </EntityLayout.Route>
  </EntityLayout>
);

const domainPage = (
  <EntityLayout>
    <EntityLayout.Route path="/" title="Overview">
      <Grid.Root columns="12" gap="3">
        {entityWarningContent}
        <Grid.Item colSpan={{ initial: '12', md: '6' }}>
          <EntityAboutCard />
        </Grid.Item>
        <Grid.Item colSpan={{ initial: '12', md: '6' }}>
          <EntityCatalogGraphCard height={400} />
        </Grid.Item>
        <Grid.Item colSpan={{ initial: '12', md: '6' }}>
          <EntityHasSystemsCard />
        </Grid.Item>
      </Grid.Root>
    </EntityLayout.Route>
  </EntityLayout>
);

export const entityPage = (
  <EntitySwitch>
    <EntitySwitch.Case if={isKind('component')} children={componentPage} />
    <EntitySwitch.Case if={isKind('api')} children={apiPage} />
    <EntitySwitch.Case if={isKind('group')} children={groupPage} />
    <EntitySwitch.Case if={isKind('user')} children={userPage} />
    <EntitySwitch.Case if={isKind('system')} children={systemPage} />
    <EntitySwitch.Case if={isKind('domain')} children={domainPage} />
    <EntitySwitch.Case>{defaultEntityPage}</EntitySwitch.Case>
  </EntitySwitch>
);
