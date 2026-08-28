import { useCallback, useEffect, useState } from 'react'
import { useNavigate, useOutletContext } from 'react-router-dom'
import { useAuth } from '../contexts/AuthContext'
import {
  getRegisteredClusters,
  deleteRegisteredCluster,
  getClusters,
  type RegisteredCluster,
} from '../api'
import type { ClusterInfo } from '../types'

interface LayoutContext {
  clusterOptions: ClusterInfo[]
}
import AdminSubNav from '../components/AdminSubNav'
import styles from './AdminLayout.module.css'

// ── Page ───────────────────────────────────────────────────────────────────

export default function ClustersPage() {
  const { permissions } = useAuth()
  const navigate = useNavigate()
  // Seed initial state from the Layout-level clusterOptions cache (fetched
  // once at app entry and shared across routes via Outlet context). On page
  // re-entry this gives us immediate Synced/Reports display instead of a
  // "—" flash until our own poll completes.
  const { clusterOptions } = useOutletContext<LayoutContext>()
  const seed: Record<string, ClusterInfo> = {}
  for (const c of clusterOptions ?? []) seed[c.name] = c
  const [clusters, setClusters] = useState<RegisteredCluster[]>([])
  const [dbClusters, setDbClusters] = useState<Record<string, ClusterInfo>>(seed)
  const [dbLoaded, setDbLoaded] = useState(Object.keys(seed).length > 0)

  const [deleteTarget, setDeleteTarget] = useState<string | null>(null)
  const [deleteBusy, setDeleteBusy] = useState(false)

  const fetchClusters = useCallback(async () => {
    // Fetch both endpoints in parallel; /api/v1/hub/clusters can take ~1.5s
    // when the server's Kubernetes client is hitting a remote API server
    // (e.g. local dev with an EKS kubeconfig), and waiting for it before
    // refreshing dbClusters caused a visible Awaiting flash.
    const [regRes, dbRes] = await Promise.allSettled([
      getRegisteredClusters(),
      getClusters(),
    ])

    if (regRes.status === 'fulfilled') {
      setClusters(Array.isArray(regRes.value) ? regRes.value : [])
    } else {
      setClusters([])
    }

    if (dbRes.status === 'fulfilled') {
      const map: Record<string, ClusterInfo> = {}
      for (const c of dbRes.value.items ?? []) map[c.name] = c
      setDbClusters(map)
      setDbLoaded(true)
    }
    // On failure keep the previous dbClusters snapshot so one flaky poll
    // doesn't flip every Synced row back to Awaiting.
  }, [])

  useEffect(() => {
    fetchClusters()
    // Poll every 10s so newly-synced clusters flip to "Synced" without manual reload
    const id = setInterval(fetchClusters, 10000)
    return () => clearInterval(id)
  }, [fetchClusters])

  if (!permissions?.can_admin) {
    return (
      <div className={styles.container}>
        <AdminSubNav />
        <div className={styles.emptyState}>Access denied. Admin permissions required.</div>
      </div>
    )
  }

  const confirmDelete = async () => {
    if (!deleteTarget) return
    setDeleteBusy(true)
    try {
      await deleteRegisteredCluster(deleteTarget)
      setDeleteTarget(null)
      fetchClusters()
    } finally {
      setDeleteBusy(false)
    }
  }

  return (
    <div className={styles.container}>
      <AdminSubNav />

      {/* Registered clusters */}
      <div className={styles.section}>
        <div className={styles.sectionHeader} style={{ display: 'flex', alignItems: 'center', gap: 12 }}>
          <h3 className={styles.sectionTitle}>Registered Clusters ({clusters.length})</h3>
          <div style={{ flex: 1 }} />
          <button
            type="button"
            className={styles.toolbarBtn}
            onClick={() => navigate('/admin/clusters/new')}
          >
            <i className="fa-solid fa-plus" /> Create
          </button>
        </div>
        <div style={{ padding: 16 }}>
          {clusters.length === 0 ? (
            <div style={{ fontSize: 13, color: 'var(--text-muted)' }}>
              No clusters registered yet.
            </div>
          ) : (
            <table className={styles.logTable}>
              <thead>
                <tr>
                  <th>Name</th><th>API Server</th><th>TLS</th><th>Reports</th>
                  <th>Status</th><th></th>
                </tr>
              </thead>
              <tbody>
                {clusters.map((c) => {
                  const info = dbClusters[c.name]
                  const synced = !!info
                    && (info.vuln_report_count > 0 || info.sbom_report_count > 0)
                  const isLocal = c.in_cluster === true
                  // Priority: if the live probe reported the cluster
                  // unreachable, surface that first. Otherwise fall back to
                  // the DB-derived Synced / Awaiting first sync state.
                  // In-cluster row is the Hub itself — ClusterWatcher on this
                  // pod is always active, so treat it as Synced regardless of
                  // whether the DB has accumulated reports yet.
                  // Heart = reachability, colour = sync state, label = probe
                  // latency. Unreachable is a cracked heart with no latency.
                  const heart = !dbLoaded
                    ? { icon: 'fa-heart', color: 'var(--text-muted)', label: '—', title: 'Loading' }
                    : c.reachable === false
                      ? { icon: 'fa-heart-crack', color: 'var(--critical)', label: 'unreachable', title: 'Unreachable' }
                      : isLocal || synced
                        ? { icon: 'fa-heart', color: 'var(--low)', label: '', title: 'Synced' }
                        : { icon: 'fa-heart', color: 'var(--medium)', label: 'awaiting sync', title: 'Awaiting first sync' }
                  const latency =
                    typeof c.reachability_latency_ms === 'number'
                      ? `${c.reachability_latency_ms} ms`
                      : isLocal ? 'in-cluster' : ''
                  return (
                    <tr key={c.name}>
                      <td>{c.name}</td>
                      <td
                        className={`${styles.mono} ${styles.cellTruncate}`}
                        title={c.server}
                      >
                        {c.server}
                      </td>
                      <td>{c.insecure ? 'insecure' : 'verified'}</td>
                      <td className={styles.mono}>
                        {info ? (
                          <span style={{ color: 'var(--text-secondary)' }}>
                            {info.vuln_report_count} vuln
                            {' / '}
                            {info.sbom_report_count} sbom
                          </span>
                        ) : (
                          <span style={{ color: 'var(--text-muted)' }}>—</span>
                        )}
                      </td>
                      <td
                        className={styles.mono}
                        title={[heart.title, c.reachability_message].filter(Boolean).join(' · ')}
                      >
                        <i className={`fa-solid ${heart.icon}`} style={{ color: heart.color, marginRight: 6 }} />
                        {latency}
                        {heart.label && (
                          <span style={{ color: 'var(--text-muted)', marginLeft: latency ? 6 : 0 }}>
                            {heart.label}
                          </span>
                        )}
                      </td>
                      <td>
                        <button
                          className={styles.toolbarBtnDanger}
                          disabled={isLocal}
                          title={isLocal
                            ? 'The Hub\'s own cluster is auto-managed and cannot be deleted'
                            : undefined}
                          style={isLocal ? { opacity: 0.4, cursor: 'not-allowed' } : undefined}
                          onClick={() => !isLocal && setDeleteTarget(c.name)}
                        >
                          Delete
                        </button>
                      </td>
                    </tr>
                  )
                })}
              </tbody>
            </table>
          )}
        </div>
      </div>

      {/* Delete confirmation modal */}
      {deleteTarget && (
        <div className={styles.overlay} onClick={() => !deleteBusy && setDeleteTarget(null)}>
          <div className={styles.modal} onClick={(e) => e.stopPropagation()}>
            <h3 className={styles.modalTitle}>Delete cluster</h3>
            <p className={styles.modalText}>
              Delete cluster <strong>{deleteTarget}</strong>?
              <br />
              This removes the Hub Secret <em>and</em> deletes all reports for
              this cluster from the Dashboard / Vulnerabilities / SBOM views.
              The read-only ServiceAccount on the Edge cluster is unchanged.
            </p>
            <div className={styles.modalActions}>
              <button
                type="button"
                className={styles.cancelBtn}
                disabled={deleteBusy}
                onClick={() => setDeleteTarget(null)}
              >
                Cancel
              </button>
              <button
                type="button"
                className={styles.dangerBtn}
                disabled={deleteBusy}
                onClick={confirmDelete}
              >
                {deleteBusy ? 'Deleting…' : 'Delete'}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  )
}
