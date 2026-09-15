import React, { useEffect, useMemo, useState } from 'react';
import { useNavigate, useParams } from 'react-router-dom';
import {
  Alert,
  Button,
  Cell,
  CellText,
  Container,
  Flex,
  Link,
  Table,
  Tag,
  TagGroup,
  Text,
  TextAreaField,
} from '@backstage/ui';
import type { ColumnConfig, TextColorStatus, TextColors } from '@backstage/ui';
import { useApi } from '@backstage/core-plugin-api';
import { useAsyncRetry } from 'react-use';
import { patApiRef } from '../../api';
import { AuditEvent, TokenState } from '../../api/types';
import { RevokeTokenDialog } from '../RevokeTokenDialog';
import {
  AccessMap,
  accessMapToScopes,
  daysUntil,
  DESCRIPTION_MAX,
  formatDateTime,
  formatRelative,
  ReadOnlyField,
  sameScopes,
  ScopeGrid,
  scopesToAccessMap,
} from '../shared';

const RECENT_EVENTS = 20;

const STATE_COLORS: Record<TokenState, TextColorStatus | TextColors> = {
  active: 'success',
  expired: 'danger',
  revoked: 'secondary',
};

interface EventRow extends AuditEvent {
  key: string;
}

const Field = ({ label, children }: { label: string; children: React.ReactNode }) => (
  <div className="pat-stat-card">
    <Text variant="body-x-small" color="secondary">
      {label}
    </Text>
    {children}
  </div>
);

/**
 * Token detail with in-place editing of description and scopes. The name and
 * lifetime are fixed at creation and cannot be changed here. Revoke and
 * delete reuse the confirmation dialog from the list page.
 */
export const TokenDetailPage = () => {
  const { id = '' } = useParams<{ id: string }>();
  const api = useApi(patApiRef);
  const navigate = useNavigate();

  const { value: token, loading, error, retry } = useAsyncRetry(() => api.getToken(id), [api, id]);
  const { value: settings } = useAsyncRetry(() => api.getSettings(), [api]);
  const { value: events, retry: retryEvents } = useAsyncRetry(
    () => api.queryAudit({ limit: RECENT_EVENTS, offset: 0, tokenId: id }),
    [api, id],
  );

  const [description, setDescription] = useState('');
  const [access, setAccess] = useState<AccessMap>({});
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const [pending, setPending] = useState<'revoke' | 'delete' | null>(null);

  useEffect(() => {
    if (!token) return;
    setDescription(token.description ?? '');
    setAccess(scopesToAccessMap(token.scopes));
  }, [token]);

  useEffect(() => {
    if (!saved) return undefined;
    const t = setTimeout(() => setSaved(false), 2_500);
    return () => clearTimeout(t);
  }, [saved]);

  const plugins = settings?.scopablePlugins ?? [];
  const scopes = useMemo(() => accessMapToScopes(access, plugins), [access, plugins]);

  const trimmedDescription = description.trim();
  const descriptionValid =
    trimmedDescription.length > 0 && trimmedDescription.length <= DESCRIPTION_MAX;
  const scopesValid = scopes.length > 0;
  const editable = token?.state === 'active';

  const dirty =
    !!token &&
    (trimmedDescription !== (token.description ?? '') || !sameScopes(scopes, token.scopes));
  const canSave = editable && dirty && descriptionValid && scopesValid && !saving;

  const refreshAll = () => {
    retry();
    retryEvents();
  };

  const handleSave = async () => {
    if (!token || !canSave) return;
    setSaving(true);
    setSaveError(null);
    try {
      const patch: Parameters<typeof api.updateToken>[1] = {};
      if (trimmedDescription !== (token.description ?? '')) patch.description = trimmedDescription;
      if (!sameScopes(scopes, token.scopes)) patch.scopes = scopes;
      await api.updateToken(token.id, patch);
      setSaved(true);
      refreshAll();
    } catch (e) {
      setSaveError(e instanceof Error ? e.message : 'Save failed');
    } finally {
      setSaving(false);
    }
  };

  const handleReset = () => {
    if (!token) return;
    setDescription(token.description ?? '');
    setAccess(scopesToAccessMap(token.scopes));
    setSaveError(null);
  };

  const eventRows: EventRow[] = useMemo(
    () => (events?.items ?? []).map(e => ({ ...e, key: String(e.id) })),
    [events],
  );

  const eventColumns: ColumnConfig<EventRow>[] = useMemo(
    () => [
      {
        id: 'time',
        label: 'Time',
        isRowHeader: true,
        defaultWidth: '1.2fr',
        minWidth: 150,
        cell: row => (
          <CellText title={formatDateTime(row.createdAt)} description={formatRelative(row.createdAt)} />
        ),
      },
      {
        id: 'event',
        label: 'Event',
        defaultWidth: '1fr',
        minWidth: 120,
        cell: row => (
          <Cell>
            <Flex direction="column" gap="0.5">
              <Text
                variant="body-small"
                weight="bold"
                color={row.outcome === 'denied' ? 'danger' : 'primary'}
              >
                {row.eventType}
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
        id: 'detail',
        label: 'Detail',
        defaultWidth: '3fr',
        minWidth: 240,
        cell: row => {
          if (row.method) {
            return (
              <Cell>
                <Flex direction="column" gap="0.5">
                  <Text variant="body-small" className="pat-mono" truncate>
                    {row.method} {row.path}
                  </Text>
                  <Text variant="body-x-small" color="secondary">
                    {row.statusCode ?? '–'}
                    {row.durationMs !== null ? ` · ${row.durationMs} ms` : ''}
                    {row.ip ? ` · ${row.ip}` : ''}
                  </Text>
                </Flex>
              </Cell>
            );
          }
          return (
            <Cell>
              <Flex direction="column" gap="0.5">
                <Text variant="body-small">by {row.actor ?? 'unknown'}</Text>
                {row.details && (
                  <Text variant="body-x-small" color="secondary" className="pat-mono" truncate>
                    {row.details}
                  </Text>
                )}
              </Flex>
            </Cell>
          );
        },
      },
    ],
    [],
  );

  if (loading && !token) {
    return (
      <Container my="4">
        <Text>Loading…</Text>
      </Container>
    );
  }

  if (error || !token) {
    return (
      <Container my="4">
        <Flex direction="column" gap="3">
          <Alert
            status="danger"
            title="Token not found"
            description={error?.message ?? 'This token may have been deleted.'}
          />
          <Link href="/pat">Back to tokens</Link>
        </Flex>
      </Container>
    );
  }

  const remaining = daysUntil(token.expiresAt);

  return (
    <Container my="4">
      <Flex direction="column" gap="4">
        <Flex justify="between" align="center" style={{ flexWrap: 'wrap' }} gap="2">
          <Flex direction="column" gap="0.5">
            <Link href="/pat">← All tokens</Link>
            <Flex align="center" gap="2">
              <Text variant="title-small" weight="bold">
                {token.name}
              </Text>
              <Text variant="body-x-small" weight="bold" color={STATE_COLORS[token.state]}>
                {token.state.toUpperCase()}
              </Text>
            </Flex>
            <Text variant="body-x-small" color="secondary" className="pat-mono">
              {token.tokenPrefix}…
            </Text>
          </Flex>
          <Flex gap="2">
            <Button variant="secondary" onPress={refreshAll}>
              Refresh
            </Button>
            {token.state === 'active' && (
              <Button variant="secondary" destructive onPress={() => setPending('revoke')}>
                Revoke
              </Button>
            )}
            <Button variant="primary" destructive onPress={() => setPending('delete')}>
              Delete
            </Button>
          </Flex>
        </Flex>

        <div className="pat-stat-grid">
          <Field label="Expires">
            <Text variant="body-small" weight="bold">
              {formatDateTime(token.expiresAt)}
            </Text>
            <Text variant="body-x-small" color="secondary">
              {token.state === 'active' ? `${Math.max(remaining, 0)}d left` : token.state}
            </Text>
          </Field>
          <Field label="Last used">
            <Text variant="body-small" weight="bold">
              {token.lastUsedAt ? formatRelative(token.lastUsedAt) : 'Never'}
            </Text>
            <Text variant="body-x-small" color="secondary">
              {token.useCount} calls{token.lastUsedIp ? ` · ${token.lastUsedIp}` : ''}
            </Text>
          </Field>
          <Field label="Created">
            <Text variant="body-small" weight="bold">
              {formatDateTime(token.createdAt)}
            </Text>
            <Text variant="body-x-small" color="secondary">
              {token.createdBy}
            </Text>
          </Field>
          {token.revokedAt && (
            <Field label="Revoked">
              <Text variant="body-small" weight="bold">
                {formatDateTime(token.revokedAt)}
              </Text>
              <Text variant="body-x-small" color="secondary">
                {token.revokedBy}
              </Text>
            </Field>
          )}
        </div>

        {!editable && (
          <Alert
            status="info"
            title={`This token is ${token.state}`}
            description="Description and permissions are read-only once a token can no longer authenticate. Issue a new token to change access."
          />
        )}

        <Flex direction="column" gap="4" style={{ maxWidth: 720 }}>
          <ReadOnlyField
            label="Name"
            value={token.name}
            hint="Fixed at creation and cannot be changed."
          />
          <ReadOnlyField
            label="ID"
            value={token.id}
            mono
            hint="Use this to filter the audit log or address the token in the API."
          />
          <TextAreaField
            label="Description"
            isRequired
            isDisabled={!editable}
            value={description}
            onChange={setDescription}
            maxLength={DESCRIPTION_MAX}
            rows={2}
            description={`${trimmedDescription.length}/${DESCRIPTION_MAX}`}
          />
          {settings ? (
            <ScopeGrid
              plugins={plugins}
              access={access}
              onChange={setAccess}
              isDisabled={!editable}
            />
          ) : (
            <TagGroup aria-label="Scopes">
              {token.scopes.map(s => (
                <Tag key={s.plugin} id={s.plugin} size="small">
                  {s.plugin}:{s.access}
                </Tag>
              ))}
            </TagGroup>
          )}

          {editable && (
            <Flex gap="2" align="center">
              <Button variant="primary" onPress={handleSave} isDisabled={!canSave} isPending={saving}>
                Save changes
              </Button>
              <Button variant="secondary" onPress={handleReset} isDisabled={!dirty || saving}>
                Reset
              </Button>
              {saved && (
                <Text variant="body-small" color="success">
                  Saved
                </Text>
              )}
              {dirty && !scopesValid && (
                <Text variant="body-x-small" color="danger">
                  At least one permission is required.
                </Text>
              )}
            </Flex>
          )}
          {saveError && <Alert status="danger" title={saveError} />}
        </Flex>

        <Flex direction="column" gap="2">
          <Text variant="body-small" weight="bold">
            Recent activity
          </Text>
          <Text variant="body-x-small" color="secondary">
            Last {RECENT_EVENTS} events for this token. The full history is in the Audit Log tab.
          </Text>
          <div className="pat-table-wrapper">
            <Table<EventRow>
              data={eventRows}
              columnConfig={eventColumns}
              pagination={{ type: 'none' }}
              emptyState={
                <Text color="secondary" style={{ padding: 16, display: 'block' }}>
                  No activity recorded yet.
                </Text>
              }
            />
          </div>
        </Flex>
      </Flex>

      {pending && (
        <RevokeTokenDialog
          token={token}
          mode={pending}
          onClose={() => setPending(null)}
          onDone={() => {
            if (pending === 'delete') {
              navigate('/pat');
            } else {
              refreshAll();
            }
          }}
        />
      )}
    </Container>
  );
};
