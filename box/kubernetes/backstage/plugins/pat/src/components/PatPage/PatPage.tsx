import React from 'react';
import { Route, Routes } from 'react-router-dom';
import { Alert, Container, PluginHeader, Tag, TagGroup, Text } from '@backstage/ui';
import { RiShieldKeyholeLine } from '@remixicon/react';
import { useApi } from '@backstage/core-plugin-api';
import { useAsync } from 'react-use';
import { patPlugin } from '../../plugin';
import { patApiRef } from '../../api';
import { TokensView } from '../TokensView';
import { AuditView } from '../AuditView';
import { TokenDetailPage } from '../TokenDetailPage';
import { CreateTokenPage } from '../CreateTokenPage';
import './PatPage.css';

const Header = () => (
  <PluginHeader
    icon={<RiShieldKeyholeLine />}
    title="Access Tokens"
    customActions={
      <TagGroup>
        <Tag id="plugin-id" size="small">
          {patPlugin.getId()}
        </Tag>
      </TagGroup>
    }
    tabs={[
      { id: 'tokens', label: 'Tokens', href: '/pat', matchStrategy: 'exact' },
      { id: 'audit', label: 'Audit Log', href: '/pat/audit', matchStrategy: 'prefix' },
    ]}
  />
);

export const PatPage = () => {
  const api = useApi(patApiRef);
  const { value: adminStatus, loading } = useAsync(() => api.getAdminStatus(), [api]);
  const isAdmin = adminStatus?.isAdmin ?? false;

  if (loading) {
    return (
      <>
        <Header />
        <Container my="4">
          <Text>Loading…</Text>
        </Container>
      </>
    );
  }

  if (!isAdmin) {
    return (
      <>
        <Header />
        <Container my="4">
          <Alert
            status="warning"
            title="Administrator access required"
            description="Personal access tokens grant API access to other systems, so only Backstage administrators can issue them or read the audit log. Ask the DevOps team to add you to the admin list if you need access."
          />
        </Container>
      </>
    );
  }

  return (
    <>
      <Header />
      <Routes>
        <Route path="/" element={<TokensView />} />
        <Route path="/tokens/new" element={<CreateTokenPage />} />
        <Route path="/tokens/:id" element={<TokenDetailPage />} />
        <Route path="/audit" element={<AuditView />} />
      </Routes>
    </>
  );
};
