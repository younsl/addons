import React from 'react';
import { Flex, Select, Text } from '@backstage/ui';
import { ScopablePlugin, ScopeAccess, TokenScope } from '../../api/types';

export type AccessChoice = ScopeAccess | 'none';
export type AccessMap = Record<string, AccessChoice>;

export function scopesToAccessMap(scopes: readonly TokenScope[]): AccessMap {
  const map: AccessMap = {};
  for (const s of scopes) map[s.plugin] = s.access;
  return map;
}

export function accessMapToScopes(
  access: AccessMap,
  plugins: readonly ScopablePlugin[],
): TokenScope[] {
  return plugins
    .filter(p => access[p.id] === 'read' || access[p.id] === 'write')
    .map(p => ({ plugin: p.id, access: access[p.id] as ScopeAccess }));
}

export function sameScopes(a: readonly TokenScope[], b: readonly TokenScope[]): boolean {
  const key = (list: readonly TokenScope[]) =>
    [...list]
      .map(s => `${s.plugin}:${s.access}`)
      .sort()
      .join(',');
  return key(a) === key(b);
}

interface Props {
  plugins: readonly ScopablePlugin[];
  access: AccessMap;
  onChange: (next: AccessMap) => void;
  isDisabled?: boolean;
}

/** Per-plugin access picker shared by the create dialog and the detail page. */
export const ScopeGrid = ({ plugins, access, onChange, isDisabled }: Props) => (
  <div>
    <div className="pat-section-heading">
      <Text variant="body-small" weight="bold">
        Permissions
      </Text>
      <Text variant="body-x-small" color="secondary">
        Grant access per plugin. Read covers GET requests only, write covers every method. At
        least one plugin is required.
      </Text>
    </div>
    <div className="pat-scope-grid" style={{ marginTop: 8 }}>
      <div className="pat-scope-grid-header">
        <Text variant="body-x-small" weight="bold" color="secondary">
          Plugin
        </Text>
      </div>
      <div className="pat-scope-grid-header">
        <Text variant="body-x-small" weight="bold" color="secondary">
          Access
        </Text>
      </div>
      {plugins.map(plugin => (
        <React.Fragment key={plugin.id}>
          <Flex direction="column" gap="0.5">
            <Text variant="body-small" weight="bold">
              {plugin.label}
            </Text>
            <Text variant="body-x-small" color="secondary" className="pat-mono">
              /api/{plugin.id}
              {plugin.description ? ` · ${plugin.description}` : ''}
            </Text>
          </Flex>
          <Select
            aria-label={`Access for ${plugin.label}`}
            size="small"
            isDisabled={isDisabled}
            selectedKey={access[plugin.id] ?? 'none'}
            onSelectionChange={key => onChange({ ...access, [plugin.id]: key as AccessChoice })}
            options={[
              { value: 'none', label: 'No access' },
              { value: 'read', label: 'Read' },
              { value: 'write', label: 'Read & write' },
            ]}
          />
        </React.Fragment>
      ))}
      {plugins.length === 0 && (
        <Text variant="body-small" color="secondary">
          No scopable plugins configured. Set pat.scopablePlugins in app-config.
        </Text>
      )}
    </div>
  </div>
);
