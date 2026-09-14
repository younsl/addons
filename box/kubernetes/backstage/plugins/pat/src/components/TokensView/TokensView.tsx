import React, { useMemo, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import {
  Alert,
  Box,
  Button,
  Cell,
  CellText,
  Container,
  Flex,
  SearchField,
  Select,
  Table,
  Tag,
  TagGroup,
  Text,
} from '@backstage/ui';
import type { ColumnConfig, SortDescriptor, TextColorStatus, TextColors } from '@backstage/ui';
import { useApi } from '@backstage/core-plugin-api';
import { useAsyncRetry } from 'react-use';
import { patApiRef } from '../../api';
import { PatToken, TokenState } from '../../api/types';
import { CreateTokenDialog } from '../CreateTokenDialog';
import { daysUntil, formatDate, formatDateTime, formatRelative } from '../shared';

interface TokenRow extends PatToken {
  key: string;
}

const STATE_COLORS: Record<TokenState, TextColorStatus | TextColors> = {
  active: 'success',
  expired: 'danger',
  revoked: 'secondary',
};

const StatCard = ({ label, value, hint }: { label: string; value: number | string; hint?: string }) => (
  <div className="pat-stat-card">
    <Text variant="body-x-small" color="secondary">
      {label}
    </Text>
    <Text variant="title-small" weight="bold">
      {value}
    </Text>
    {hint && (
      <Text variant="body-x-small" color="secondary">
        {hint}
      </Text>
    )}
  </div>
);

export const TokensView = () => {
  const api = useApi(patApiRef);
  const navigate = useNavigate();
  const { value: settings, error: settingsError } = useAsyncRetry(() => api.getSettings(), [api]);
  const {
    value: tokens,
    loading,
    error,
    retry,
  } = useAsyncRetry(() => api.listTokens(), [api]);
  const { value: summary, retry: retrySummary } = useAsyncRetry(
    () => api.getAuditSummary(),
    [api],
  );

  const [search, setSearch] = useState('');
  const [stateFilter, setStateFilter] = useState<'all' | TokenState>('all');
  const [creating, setCreating] = useState(false);
  const [sort, setSort] = useState<SortDescriptor>({ column: 'created', direction: 'descending' });

  const refresh = () => {
    retry();
    retrySummary();
  };

  const rows: TokenRow[] = useMemo(() => {
    const lower = search.trim().toLowerCase();
    const list = (tokens ?? [])
      .filter(t => stateFilter === 'all' || t.state === stateFilter)
      .filter(
        t =>
          !lower ||
          t.name.toLowerCase().includes(lower) ||
          (t.description ?? '').toLowerCase().includes(lower) ||
          t.createdBy.toLowerCase().includes(lower) ||
          t.scopes.some(s => s.plugin.includes(lower)),
      )
      .map(t => ({ ...t, key: t.id }));
    const dir = sort.direction === 'descending' ? -1 : 1;
    const col = String(sort.column ?? 'created');
    return list.sort((a, b) => {
      switch (col) {
        case 'name':
          return a.name.localeCompare(b.name) * dir;
        case 'state':
          return a.state.localeCompare(b.state) * dir;
        case 'expires':
          return (new Date(a.expiresAt).getTime() - new Date(b.expiresAt).getTime()) * dir;
        case 'lastUsed': {
          const at = a.lastUsedAt ? new Date(a.lastUsedAt).getTime() : 0;
          const bt = b.lastUsedAt ? new Date(b.lastUsedAt).getTime() : 0;
          return (at - bt) * dir;
        }
        case 'created':
        default:
          return (new Date(a.createdAt).getTime() - new Date(b.createdAt).getTime()) * dir;
      }
    });
  }, [tokens, search, stateFilter, sort]);

  const columns: ColumnConfig<TokenRow>[] = useMemo(
    () => [
      {
        id: 'name',
        label: 'Name',
        isRowHeader: true,
        isSortable: true,
        defaultWidth: '2fr',
        minWidth: 160,
        cell: row => (
          <Cell>
            <Flex direction="column" gap="0.5">
              <Text variant="body-small" weight="bold" truncate>
                {row.name}
              </Text>
              <Text variant="body-x-small" color="secondary" className="pat-mono">
                {row.tokenPrefix}…
              </Text>
              {row.description && (
                <Text variant="body-x-small" color="secondary" truncate>
                  {row.description}
                </Text>
              )}
            </Flex>
          </Cell>
        ),
      },
      {
        id: 'scopes',
        label: 'Permissions',
        defaultWidth: '2fr',
        minWidth: 160,
        cell: row => (
          <Cell>
            <TagGroup aria-label="Scopes">
              {row.scopes.map(s => (
                <Tag key={`${s.plugin}:${s.access}`} size="small" id={`${row.id}-${s.plugin}`}>
                  {s.plugin}:{s.access}
                </Tag>
              ))}
            </TagGroup>
          </Cell>
        ),
      },
      {
        id: 'state',
        label: 'State',
        isSortable: true,
        defaultWidth: 90,
        minWidth: 80,
        cell: row => (
          <Cell>
            <Text variant="body-x-small" weight="bold" color={STATE_COLORS[row.state]}>
              {row.state.toUpperCase()}
            </Text>
          </Cell>
        ),
      },
      {
        id: 'expires',
        label: 'Expires',
        isSortable: true,
        defaultWidth: '1fr',
        minWidth: 110,
        cell: row => {
          const d = daysUntil(row.expiresAt);
          const hint = row.state !== 'active' ? row.state : d <= 0 ? 'today' : `${d}d left`;
          return <CellText title={formatDate(row.expiresAt)} description={hint} />;
        },
      },
      {
        id: 'lastUsed',
        label: 'Last used',
        isSortable: true,
        defaultWidth: '1fr',
        minWidth: 120,
        cell: row => (
          <CellText
            title={row.lastUsedAt ? formatRelative(row.lastUsedAt) : 'Never'}
            description={
              row.lastUsedAt
                ? `${row.useCount} calls${row.lastUsedIp ? ` · ${row.lastUsedIp}` : ''}`
                : undefined
            }
          />
        ),
      },
      {
        id: 'created',
        label: 'Created',
        isSortable: true,
        defaultWidth: '1.2fr',
        minWidth: 130,
        cell: row => <CellText title={formatDateTime(row.createdAt)} description={row.createdBy} />,
      },
    ],
    [],
  );

  const totals = useMemo(() => {
    const list = tokens ?? [];
    return {
      total: list.length,
      active: list.filter(t => t.state === 'active').length,
      revoked: list.filter(t => t.state === 'revoked').length,
      expired: list.filter(t => t.state === 'expired').length,
    };
  }, [tokens]);

  return (
    <Container my="4">
      <Flex direction="column" gap="4">
        <div className="pat-stat-grid">
          <StatCard label="Active tokens" value={totals.active} hint={`${totals.total} total`} />
          <StatCard
            label="Expiring within 30d"
            value={summary?.expiringSoonTokens ?? '–'}
            hint="active tokens"
          />
          <StatCard
            label="API calls (24h)"
            value={summary?.requests ?? '–'}
            hint="authenticated with a token"
          />
          <StatCard
            label="Denied (24h)"
            value={summary?.denied ?? '–'}
            hint="invalid, expired or out of scope"
          />
        </div>

        <Flex gap="2" align="center" justify="between" style={{ flexWrap: 'wrap' }}>
          <Flex gap="2" align="center" style={{ flexWrap: 'wrap' }}>
            <SearchField
              aria-label="Search tokens"
              placeholder="Search name, description, owner, plugin"
              value={search}
              onChange={setSearch}
            />
            <Select
              aria-label="State filter"
              selectedKey={stateFilter}
              onSelectionChange={key => setStateFilter(key as any)}
              options={[
                { value: 'all', label: 'All states' },
                { value: 'active', label: 'Active' },
                { value: 'expired', label: 'Expired' },
                { value: 'revoked', label: 'Revoked' },
              ]}
            />
            <Button variant="secondary" onPress={refresh}>
              Refresh
            </Button>
          </Flex>
          <Button
            variant="primary"
            onPress={() => setCreating(true)}
            isDisabled={!settings}
          >
            Create token
          </Button>
        </Flex>

        {settingsError && (
          <Alert status="danger" title="Failed to load token settings" description={settingsError.message} />
        )}
        {settings && settings.scopablePlugins.length === 0 && (
          <Alert
            status="warning"
            title="No scopable plugins configured"
            description="pat.scopablePlugins is an empty list in app-config. Remove the key to expose every plugin, or list the plugins tokens may reach."
          />
        )}

        <Box className="pat-table-wrapper">
          <Table<TokenRow>
            data={rows}
            loading={loading}
            error={error ?? undefined}
            columnConfig={columns}
            sort={{ descriptor: sort, onSortChange: setSort }}
            rowConfig={{ onClick: row => navigate(`/pat/tokens/${encodeURIComponent(row.id)}`) }}
            pagination={{ type: 'none' }}
            emptyState={
              <Box p="4">
                <Text color="secondary">
                  {tokens && tokens.length === 0
                    ? 'No tokens issued yet.'
                    : 'No tokens match the current filters.'}
                </Text>
              </Box>
            }
          />
        </Box>
      </Flex>

      {creating && settings && (
        <CreateTokenDialog
          settings={settings}
          onClose={() => setCreating(false)}
          onCreated={refresh}
        />
      )}
    </Container>
  );
};
