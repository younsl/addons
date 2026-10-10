import { useLocation } from 'react-router-dom'

/**
 * Description line for the current admin page.
 *
 * This used to be a strip of tabs, which meant reaching Alerts required first
 * landing on Clusters. Those destinations are sidebar entries now, so what is
 * left worth keeping is the sentence explaining what the page is for.
 */
const DESCRIPTIONS: { prefix: string; title: string; description: string }[] = [
  // Most specific prefix first: find() returns the first match.
  {
    prefix: '/admin/clusters/new',
    title: 'Register cluster',
    description:
      'Bootstrap a read-only ServiceAccount on the edge cluster, then register its credentials on the hub.',
  },
  {
    prefix: '/admin/clusters',
    title: 'Clusters',
    description:
      'Register edge clusters for hub-pull mode and review their report sync status.',
  },
  {
    prefix: '/admin/alerts',
    title: 'Alerts',
    description:
      'Define ConfigMap-backed alert rules and route matching findings to Slack receivers.',
  },
]

export default function AdminSubNav() {
  const { pathname } = useLocation()
  const current = DESCRIPTIONS.find((d) => pathname.startsWith(d.prefix))

  if (!current) return null

  return (
    <div style={{ marginBottom: 16 }}>
      <h2
        style={{
          margin: 0,
          fontSize: 16,
          fontWeight: 600,
          letterSpacing: '-0.2px',
          color: 'var(--text-primary)',
        }}
      >
        {current.title}
      </h2>
      <p
        style={{
          margin: '4px 0 0',
          fontSize: 12,
          lineHeight: 1.5,
          color: 'var(--text-muted)',
        }}
      >
        {current.description}
      </p>
    </div>
  )
}
