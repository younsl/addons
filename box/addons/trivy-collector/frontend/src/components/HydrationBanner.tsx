import { useCallback } from 'react'
import { getHydration } from '../api'
import { usePolling } from '../hooks/usePolling'
import type { HydrationStatus } from '../types'

/**
 * Explains an empty report set, when there is something to explain.
 *
 * Report rows live on the scraper's ephemeral volume and are rebuilt from the
 * clusters that own the reports, so "no findings" and "not loaded yet" would
 * otherwise look identical. They are not the same answer, and neither is "no
 * cluster is being watched at all", which never resolves on its own and so
 * needs different wording from a rebuild.
 */
export default function HydrationBanner() {
  const fetcher = useCallback(() => getHydration(), [])
  const { data } = usePolling<HydrationStatus>(fetcher, 5000)

  if (!data) return null

  // Nothing is being watched: the empty table is the final answer, and the fix
  // is configuration rather than waiting.
  if (!data.watching) {
    return (
      <Notice tone="info">
        <strong>No clusters are being watched.</strong> The scraper has no local
        watcher and no registered edge clusters, so there are no reports to
        collect. Enable <code>scraper.watchLocal</code> or register a cluster
        under Admin.
      </Notice>
    )
  }

  if (data.hydrated) return null

  const clusters = Object.entries(data.clusters)
  const done = clusters.filter(
    ([, c]) => c.vuln_initial_sync_done && c.sbom_initial_sync_done,
  ).length
  const pending = clusters
    .filter(([, c]) => !c.vuln_initial_sync_done || !c.sbom_initial_sync_done)
    .map(([name]) => name)

  return (
    <Notice tone="warning">
      <strong>Rebuilding report data.</strong>{' '}
      {clusters.length > 0 ? (
        <>
          {done} of {clusters.length} cluster{clusters.length === 1 ? '' : 's'}{' '}
          synced.
          {pending.length > 0 && <> Waiting on {pending.join(', ')}.</>}
        </>
      ) : (
        <>Waiting for the scraper to report which clusters it watches.</>
      )}{' '}
      Counts and tables below are incomplete until this clears.
    </Notice>
  )
}

function Notice({
  tone,
  children,
}: {
  tone: 'info' | 'warning'
  children: React.ReactNode
}) {
  const border = tone === 'warning' ? 'var(--high)' : 'var(--accent)'
  return (
    <div
      role="status"
      style={{
        margin: '0 0 16px',
        padding: '10px 14px',
        border: `1px solid ${border}`,
        borderRadius: 6,
        background: 'var(--bg-secondary)',
        fontSize: 13,
        lineHeight: 1.5,
      }}
    >
      {children}
    </div>
  )
}
