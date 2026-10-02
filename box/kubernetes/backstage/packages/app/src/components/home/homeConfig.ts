import React from 'react';
import {
  KubernetesIcon,
  ArgocdIcon,
  CategoryIcon,
  ExtensionIcon,
  CloudUploadIcon,
  CostIcon,
} from './icons';

export interface QuickLinkItem {
  url: string;
  label: string;
  Icon: React.FC<{ style?: React.CSSProperties }>;
  description: string;
  badge?: number;
}

export const quickLinks: QuickLinkItem[] = [
  // Resources
  { url: '/platforms', label: 'Platforms', Icon: KubernetesIcon, description: 'Internal platform services' },
  { url: '/catalog', label: 'Catalog', Icon: CategoryIcon, description: 'Browse all registered entities' },
  { url: '/api-docs', label: 'APIs', Icon: ExtensionIcon, description: 'Explore API documentation' },
  { url: '/openapi-registry', label: 'API Registry', Icon: CloudUploadIcon, description: 'Upload and manage OpenAPI specs' },
  // Operations
  { url: '/argocd-appset', label: 'ArgoCD', Icon: ArgocdIcon, description: 'Manage ArgoCD ApplicationSets' },
  { url: '/cost-report', label: 'Cost Report', Icon: CostIcon, description: 'View cluster cost breakdown' },
];

export const searchTypeLabels: Record<string, string> = {
  'software-catalog': 'Catalog',
  techdocs: 'Docs',
  'api-docs': 'API',
};

export const searchTypeBadgeColors: Record<string, string> = {
  'software-catalog': '#3b82f6',
  techdocs: '#10b981',
  'api-docs': '#8b5cf6',
};
