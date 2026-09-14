import { createApiRef } from '@backstage/core-plugin-api';
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

export interface PatApi {
  getAdminStatus(): Promise<{ isAdmin: boolean }>;
  getSettings(): Promise<PatSettings>;
  listTokens(): Promise<PatToken[]>;
  getToken(id: string): Promise<PatToken>;
  createToken(input: CreateTokenInput): Promise<CreatedToken>;
  updateToken(id: string, input: UpdateTokenInput): Promise<PatToken>;
  revokeToken(id: string): Promise<PatToken>;
  deleteToken(id: string): Promise<void>;
  queryAudit(query: AuditQuery): Promise<AuditPage>;
  getAuditSummary(): Promise<AuditSummary>;
}

export const patApiRef = createApiRef<PatApi>({
  id: 'plugin.pat.api',
});
