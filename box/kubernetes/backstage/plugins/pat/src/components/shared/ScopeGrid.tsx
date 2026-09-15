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

const ACCESS_OPTIONS = [
  { value: 'none', label: 'No access' },
  { value: 'read', label: 'Read' },
  { value: 'write', label: 'Read & write' },
];

export function uniformAccess(
  access: AccessMap,
  plugins: readonly ScopablePlugin[],
): AccessChoice | null {
  if (plugins.length === 0) return null;
  const first = access[plugins[0].id] ?? 'none';
  return plugins.every(p => (access[p.id] ?? 'none') === first) ? first : null;
}

export function applyToAll(
  plugins: readonly ScopablePlugin[],
  choice: AccessChoice,
): AccessMap {
  const map: AccessMap = {};
  for (const p of plugins) map[p.id] = choice;
  return map;
}

interface Props {
  plugins: readonly ScopablePlugin[];
  access: AccessMap;
  onChange: (next: AccessMap) => void;
  isDisabled?: boolean;
}

/** Per-plugin access picker shared by the create dialog and the detail page. */
export const ScopeGrid = ({ plugins, access, onChange, isDisabled }: Props) => {
  const uniform = uniformAccess(access, plugins);
  return (
    <div>
      <div className="pat-section-heading">
        <Text variant="body-small" weight="bold">
          Permissions
        </Text>
        <Text variant="body-x-small" color="secondary">
          Grant access per plugin, or use the picker in the Access column to set every plugin at
          once. Read covers GET requests only, write covers every method. At least one plugin is
          required.
        </Text>
      </div>
      <div className="pat-scope-grid" style={{ marginTop: 8 }}>
        <div className="pat-scope-grid-header">
          <Text variant="body-x-small" weight="bold" color="secondary">
            Plugin
          </Text>
        </div>
        <div className="pat-scope-grid-header">
          <Select
            aria-label="Access for all plugins"
            size="small"
            isDisabled={isDisabled || plugins.length === 0}
            placeholder="Set all…"
            selectedKey={uniform}
            onSelectionChange={key => key && onChange(applyToAll(plugins, key as AccessChoice))}
            options={ACCESS_OPTIONS}
          />
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
              options={ACCESS_OPTIONS}
            />
          </React.Fragment>
        ))}
        {plugins.length === 0 && (
          <Text variant="body-small" color="secondary">
            pat.scopablePlugins is an empty list in app-config. Remove the key to expose every plugin, or list the plugins tokens may reach.
          </Text>
        )}
      </div>
    </div>
  );
};
