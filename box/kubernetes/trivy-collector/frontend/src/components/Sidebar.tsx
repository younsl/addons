import { Link, useLocation } from 'react-router-dom'
import { useAuth } from '../contexts/AuthContext'
import { logout } from '../auth'
import { useSidebarCollapsed } from '../hooks/useSidebarCollapsed'
import styles from './Sidebar.module.css'
import type { AuthPermissions, VersionResponse } from '../types'

/** Where the embedded OpenAPI reference is mounted by the server. */
const API_DOCS_PATH = '/api-docs'

/** Paths that live under another entry's prefix and must match exactly. */
const SEARCH_PATHS = ['/vulnerabilities/search', '/sbom/components']

interface NavEntry {
  to: string
  label: string
  icon: string
  /** True when the entry should highlight for nested routes too. */
  prefix?: boolean
  /** Opens outside the SPA router. */
  external?: boolean
  visible?: (p: AuthPermissions | null | undefined) => boolean
}

interface NavGroup {
  title: string
  entries: NavEntry[]
}

/**
 * Menus regrouped by what the reader is trying to do rather than by the order
 * the pages were built in. Search used to be reachable only from a button
 * inside the reports table, and the admin pages only from a second row of tabs
 * once you were already there; both are top-level destinations now.
 */
const NAV_GROUPS: NavGroup[] = [
  {
    title: 'Overview',
    entries: [{ to: '/dashboard', label: 'Dashboard', icon: 'fa-solid fa-chart-line' }],
  },
  {
    title: 'Reports',
    entries: [
      {
        to: '/vulnerabilities',
        label: 'Vulnerabilities',
        icon: 'fa-solid fa-bug',
        prefix: true,
      },
      { to: '/sbom', label: 'SBOM', icon: 'fa-solid fa-cubes', prefix: true },
    ],
  },
  {
    title: 'Search',
    entries: [
      {
        to: '/vulnerabilities/search',
        label: 'CVE Search',
        icon: 'fa-solid fa-magnifying-glass',
      },
      {
        to: '/sbom/components',
        label: 'Component Search',
        icon: 'fa-solid fa-magnifying-glass-chart',
      },
    ],
  },
  {
    title: 'Access',
    entries: [{ to: '/auth', label: 'API Tokens', icon: 'fa-solid fa-key' }],
  },
  {
    // Flattened out of the admin tab strip, so an operator can reach either
    // page without first landing on the other.
    title: 'Admin',
    entries: [
      {
        to: '/admin/clusters',
        label: 'Clusters',
        icon: 'fa-solid fa-server',
        visible: (p) => !!p?.can_view_clusters,
      },
      {
        to: '/admin/alerts',
        label: 'Alerts',
        icon: 'fa-solid fa-bell',
        visible: (p) => !!p?.can_view_alerts,
      },
    ],
  },
  {
    title: 'Reference',
    entries: [
      {
        to: API_DOCS_PATH,
        label: 'API Docs',
        icon: 'fa-solid fa-book',
        external: true,
      },
    ],
  },
]

interface SidebarProps {
  version: VersionResponse | null
}

export default function Sidebar({ version }: SidebarProps) {
  const { pathname } = useLocation()
  const { authMode, authenticated, user, permissions } = useAuth()
  const [collapsed, toggle] = useSidebarCollapsed()

  const className = collapsed ? `${styles.sidebar} ${styles.collapsed}` : styles.sidebar

  /**
   * Exact match by default. `prefix` opts an entry into matching its nested
   * routes, but the search pages sit under those same prefixes, so a plain
   * `startsWith` would light up two entries at once.
   */
  const isActive = (entry: NavEntry) => {
    if (entry.external) return false
    if (SEARCH_PATHS.includes(pathname)) return entry.to === pathname
    return entry.prefix ? pathname.startsWith(entry.to) : pathname === entry.to
  }

  const groups = NAV_GROUPS.map((group) => ({
    ...group,
    entries: group.entries.filter((e) => !e.visible || e.visible(permissions)),
  })).filter((group) => group.entries.length > 0)

  return (
    <aside className={className}>
      <div className={styles.brand}>
        <Link to="/dashboard" className={styles.brandLink} title="Trivy Collector">
          <span className={styles.brandMark} aria-hidden="true">
            <i className="fa-solid fa-shield-halved" />
          </span>
          {!collapsed && (
            <span className={styles.brandText}>
              <span className={styles.brandName}>Trivy Collector</span>
              {version && (
                <Link
                  to="/version"
                  className={styles.brandVersion}
                  title="View detailed version info"
                >
                  v{version.version} ({version.commit.substring(0, 7)})
                </Link>
              )}
            </span>
          )}
        </Link>
        <button
          type="button"
          className={styles.foldButton}
          onClick={toggle}
          title={collapsed ? 'Expand sidebar' : 'Collapse sidebar'}
          aria-label={collapsed ? 'Expand sidebar' : 'Collapse sidebar'}
          aria-expanded={!collapsed}
        >
          <i
            className={collapsed ? 'fa-solid fa-angles-right' : 'fa-solid fa-angles-left'}
            aria-hidden="true"
          />
        </button>
      </div>

      <nav className={styles.nav}>
        {groups.map((group) => (
          <section key={group.title} className={styles.group} aria-label={group.title}>
            <div className={styles.groupTitle}>{group.title}</div>
            {group.entries.map((entry) => (
              <NavLink key={entry.to} entry={entry} active={isActive(entry)} collapsed={collapsed} />
            ))}
          </section>
        ))}
      </nav>

      <div className={styles.spacer} />

      {authMode === 'keycloak' && authenticated && user && (
        <div className={styles.footer}>
          <div className={styles.user}>
            <div className={styles.userDetails}>
              <span className={styles.userName}>
                {user.name ?? user.preferred_username ?? user.email ?? user.sub}
              </span>
              <span
                className={styles.userGroups}
                title={user.groups.length > 0 ? user.groups.join(', ') : 'No groups assigned'}
              >
                {user.groups.length > 0 ? user.groups.join(', ') : 'No groups'}
              </span>
            </div>
            <button
              type="button"
              className={styles.logoutButton}
              onClick={logout}
              title="Logout"
              aria-label="Logout"
            >
              <i className="fa-solid fa-right-from-bracket" aria-hidden="true" />
            </button>
          </div>
        </div>
      )}
    </aside>
  )
}

function NavLink({
  entry,
  active,
  collapsed,
}: {
  entry: NavEntry
  active: boolean
  collapsed: boolean
}) {
  const className = active ? `${styles.navItem} ${styles.active}` : styles.navItem
  // Only the icon survives the fold, so the label has to move into a tooltip.
  const title = collapsed ? entry.label : undefined

  const body = (
    <>
      <i className={`${entry.icon} ${styles.navIcon}`} aria-hidden="true" />
      <span className={styles.navLabel}>{entry.label}</span>
      {entry.external && (
        <i className={`fa-solid fa-arrow-up-right-from-square ${styles.externalMark}`} aria-hidden="true" />
      )}
    </>
  )

  // The API reference is served by the backend, not by the router, so it has to
  // be a real navigation rather than a client-side route change.
  if (entry.external) {
    return (
      <a
        className={className}
        href={entry.to}
        target="_blank"
        rel="noreferrer"
        title={title ?? `${entry.label} (opens in a new tab)`}
      >
        {body}
      </a>
    )
  }

  return (
    <Link className={className} to={entry.to} title={title}>
      {body}
    </Link>
  )
}
