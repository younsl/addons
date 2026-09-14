import React, { useCallback, useEffect, useMemo, useState } from 'react';
import {
  Box,
  Button,
  Cell,
  CellText,
  Container,
  Flex,
  SearchField,
  Select,
  Switch,
  Table,
  Text,
} from '@backstage/ui';
import type { ColumnConfig, TextColorStatus, TextColors } from '@backstage/ui';
import { useApi } from '@backstage/core-plugin-api';
import { useAsyncRetry } from 'react-use';
import { patApiRef } from '../../api';
import { AuditEvent, AuditEventType, AuditOutcome } from '../../api/types';
import { formatDateTime, formatRelative } from '../shared';

interface AuditRow extends AuditEvent {
  key: string;
}

const PAGE_SIZE = 50;
const AUTO_REFRESH_MS = 15_000;

const EVENT_LABEL: Record<AuditEventType, string> = {
  'token.created': 'Token created',
  'token.updated': 'Token updated',
  'token.revoked': 'Token revoked',
  'token.deleted': 'Token deleted',
  'api.request': 'API call',
  'api.denied': 'API denied',
};

const statusColor = (row: AuditEvent): TextColorStatus | TextColors => {
  if (row.outcome === 'denied') return 'danger';
  if (row.statusCode === null) return 'primary';
  if (row.statusCode >= 500) return 'danger';
  if (row.statusCode >= 400) return 'warning';
  return 'success';
};

export const AuditView = () => {
  const api = useApi(patApiRef);
  const [eventType, setEventType] = useState<'all' | AuditEventType>('all');
  const [outcome, setOutcome] = useState<'all' | AuditOutcome>('all');
  const [search, setSearch] = useState('');
  const [debouncedSearch, setDebouncedSearch] = useState('');
  const [offset, setOffset] = useState(0);
  const [autoRefresh, setAutoRefresh] = useState(false);

  useEffect(() => {
    const id = setTimeout(() => {
      setDebouncedSearch(search.trim());
      setOffset(0);
    }, 300);
    return () => clearTimeout(id);
  }, [search]);

  const { value, loading, error, retry } = useAsyncRetry(
    () =>
      api.queryAudit({
        limit: PAGE_SIZE,
        offset,
        eventType: eventType === 'all' ? undefined : eventType,
        outcome: outcome === 'all' ? undefined : outcome,
        search: debouncedSearch || undefined,
      }),
    [api, offset, eventType, outcome, debouncedSearch],
  );

  useEffect(() => {
    if (!autoRefresh) return undefined;
    const id = setInterval(retry, AUTO_REFRESH_MS);
    return () => clearInterval(id);
  }, [autoRefresh, retry]);

  const { value: tokens } = useAsyncRetry(() => api.listTokens(), [api]);
  const { value: settings } = useAsyncRetry(() => api.getSettings(), [api]);
  const tokenNames = useMemo(() => {
    const map = new Map<string, string>();
    for (const t of tokens ?? []) map.set(t.id, t.name);
    return map;
  }, [tokens]);

  const rows: AuditRow[] = useMemo(
    () => (value?.items ?? []).map(e => ({ ...e, key: String(e.id) })),
    [value],
  );
  const total = value?.total ?? 0;

  const changeFilter = useCallback((fn: () => void) => {
    fn();
    setOffset(0);
  }, []);

  const columns: ColumnConfig<AuditRow>[] = useMemo(
    () => [
      {
        id: 'time',
        label: 'Time',
        isRowHeader: true,
        defaultWidth: '1.3fr',
        minWidth: 150,
        cell: row => (
          <CellText title={formatDateTime(row.createdAt)} description={formatRelative(row.createdAt)} />
        ),
      },
      {
        id: 'event',
        label: 'Event',
        defaultWidth: '1fr',
        minWidth: 110,
        cell: row => (
          <Cell>
            <Flex direction="column" gap="0.5">
              <Text variant="body-small" weight="bold" color={row.outcome === 'denied' ? 'danger' : 'primary'}>
                {EVENT_LABEL[row.eventType] ?? row.eventType}
              </Text>
              {row.reason && (
                <Text variant="body-x-small" color="secondary">
                  {row.reason}
                </Text>
              )}
            </Flex>
          </Cell>
        ),
      },
      {
        id: 'token',
        label: 'Token',
        defaultWidth: '1.3fr',
        minWidth: 130,
        cell: row => (
          <CellText
            title={row.tokenName ?? (row.tokenId ? tokenNames.get(row.tokenId) ?? row.tokenId : '–')}
            description={row.eventType.startsWith('token.') ? `by ${row.actor ?? 'unknown'}` : row.tokenId ?? undefined}
          />
        ),
      },
      {
        id: 'request',
        label: 'Request',
        defaultWidth: '2.5fr',
        minWidth: 220,
        cell: row =>
          row.method ? (
            <Cell>
              <Flex direction="column" gap="0.5">
                <Text variant="body-small" className="pat-mono" truncate>
                  {row.method} {row.path}
                </Text>
                <Text variant="body-x-small" color="secondary">
                  plugin {row.pluginId ?? '?'}
                  {row.durationMs !== null ? ` · ${row.durationMs} ms` : ''}
                </Text>
              </Flex>
            </Cell>
          ) : (
            <CellText title="–" />
          ),
      },
      {
        id: 'status',
        label: 'Status',
        defaultWidth: 80,
        minWidth: 70,
        cell: row => (
          <Cell>
            <Text variant="body-small" weight="bold" color={statusColor(row)}>
              {row.statusCode ?? '–'}
            </Text>
          </Cell>
        ),
      },
      {
        id: 'client',
        label: 'Client',
        defaultWidth: '1.5fr',
        minWidth: 140,
        cell: row => (
          <CellText title={row.ip ?? '–'} description={row.userAgent ?? undefined} />
        ),
      },
    ],
    [tokenNames],
  );

  return (
    <Container my="4">
      <Flex direction="column" gap="3">
        <Flex gap="2" align="center" style={{ flexWrap: 'wrap' }} justify="between">
          <Flex gap="2" align="center" style={{ flexWrap: 'wrap' }}>
            <SearchField
              aria-label="Search audit log"
              placeholder="Search path, token, actor, plugin"
              value={search}
              onChange={setSearch}
            />
            <Select
              aria-label="Event type"
              selectedKey={eventType}
              onSelectionChange={key => changeFilter(() => setEventType(key as any))}
              options={[
                { value: 'all', label: 'All events' },
                { value: 'api.request', label: 'API calls' },
                { value: 'api.denied', label: 'API denied' },
                { value: 'token.created', label: 'Token created' },
                { value: 'token.updated', label: 'Token updated' },
                { value: 'token.revoked', label: 'Token revoked' },
                { value: 'token.deleted', label: 'Token deleted' },
              ]}
            />
            <Select
              aria-label="Outcome"
              selectedKey={outcome}
              onSelectionChange={key => changeFilter(() => setOutcome(key as any))}
              options={[
                { value: 'all', label: 'Any outcome' },
                { value: 'allowed', label: 'Allowed' },
                { value: 'denied', label: 'Denied' },
              ]}
            />
            <Button variant="secondary" onPress={retry}>
              Refresh
            </Button>
          </Flex>
          <Switch
            label={`Auto refresh (${AUTO_REFRESH_MS / 1000}s)`}
            isSelected={autoRefresh}
            onChange={setAutoRefresh}
          />
        </Flex>

        <Flex gap="2" align="center" justify="between" style={{ flexWrap: 'wrap' }}>
          <Text variant="body-x-small" color="secondary">
            {total > 0 ? `${total} events` : 'No events'}
            {settings
              ? ` · Retention ${settings.auditRetentionDays} days. Older events are purged daily, so export anything you need to keep beyond that.`
              : ''}
          </Text>
        </Flex>

        <Box className="pat-table-wrapper">
          <Table<AuditRow>
            data={rows}
            loading={loading}
            error={error ?? undefined}
            columnConfig={columns}
            pagination={{
              type: 'page',
              pageSize: PAGE_SIZE,
              offset,
              totalCount: total,
              hasNextPage: offset + PAGE_SIZE < total,
              hasPreviousPage: offset > 0,
              onNextPage: () => setOffset(o => o + PAGE_SIZE),
              onPreviousPage: () => setOffset(o => Math.max(0, o - PAGE_SIZE)),
              showPageSizeOptions: false,
            }}
            emptyState={
              <Box p="4">
                <Text color="secondary">No audit events match the current filters.</Text>
              </Box>
            }
          />
        </Box>
      </Flex>
    </Container>
  );
};
