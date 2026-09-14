import { DiscoveryApi, FetchApi } from '@backstage/core-plugin-api';
import { ResponseError } from '@backstage/errors';
import { PatApi } from './PatApi';
import {
  AuditPage,
  AuditQuery,
  AuditSummary,
  CreatedToken,
  CreateTokenInput,
  PatSettings,
  PatToken,
  UpdateTokenInput,
} from './types';

export class PatClient implements PatApi {
  private readonly discoveryApi: DiscoveryApi;
  private readonly fetchApi: FetchApi;

  constructor(options: { discoveryApi: DiscoveryApi; fetchApi: FetchApi }) {
    this.discoveryApi = options.discoveryApi;
    this.fetchApi = options.fetchApi;
  }

  private async request<T>(path: string, init?: RequestInit): Promise<T> {
    const base = await this.discoveryApi.getBaseUrl('pat');
    const res = await this.fetchApi.fetch(`${base}${path}`, init);
    if (!res.ok) {
      throw await ResponseError.fromResponse(res as any);
    }
    if (res.status === 204) return undefined as T;
    return res.json();
  }

  async getAdminStatus(): Promise<{ isAdmin: boolean }> {
    try {
      return await this.request<{ isAdmin: boolean }>('/admin-status');
    } catch {
      return { isAdmin: false };
    }
  }

  getSettings(): Promise<PatSettings> {
    return this.request<PatSettings>('/settings');
  }

  listTokens(): Promise<PatToken[]> {
    return this.request<PatToken[]>('/tokens');
  }

  getToken(id: string): Promise<PatToken> {
    return this.request<PatToken>(`/tokens/${encodeURIComponent(id)}`);
  }

  updateToken(id: string, input: UpdateTokenInput): Promise<PatToken> {
    return this.request<PatToken>(`/tokens/${encodeURIComponent(id)}`, {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(input),
    });
  }

  createToken(input: CreateTokenInput): Promise<CreatedToken> {
    return this.request<CreatedToken>('/tokens', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(input),
    });
  }

  revokeToken(id: string): Promise<PatToken> {
    return this.request<PatToken>(`/tokens/${encodeURIComponent(id)}/revoke`, {
      method: 'POST',
    });
  }

  deleteToken(id: string): Promise<void> {
    return this.request<void>(`/tokens/${encodeURIComponent(id)}`, { method: 'DELETE' });
  }

  queryAudit(query: AuditQuery): Promise<AuditPage> {
    const params = new URLSearchParams();
    params.set('limit', String(query.limit));
    params.set('offset', String(query.offset));
    if (query.tokenId) params.set('tokenId', query.tokenId);
    if (query.eventType) params.set('eventType', query.eventType);
    if (query.outcome) params.set('outcome', query.outcome);
    if (query.search) params.set('search', query.search);
    return this.request<AuditPage>(`/audit?${params.toString()}`);
  }

  getAuditSummary(): Promise<AuditSummary> {
    return this.request<AuditSummary>('/audit/summary');
  }
}
