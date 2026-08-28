import { useCallback } from 'react'
import { getHydration } from '../api'
import { usePolling } from '../hooks/usePolling'
import type { HydrationStatus } from '../types'

/**
 * Rebuilding notice for the window between a scraper restart and the last
 * initial sync.
 *
 * Report rows live on the scraper's ephemeral volume and are rebuilt from the
 * clusters that own the reports, so during a rebuild an empty table is not an
 * answer. Without this, "no findings" and "not loaded yet" look identical,
 * which is worse than a brief error.
 */
export default function HydrationBanner() {
  const fetcher = useCallback(() => getHydration(), [])
  const { data } = usePolling<HydrationStatus>(fetcher, 5000)

  if (!data || data.hydrated) return null

  const clusters = Object.entries(data.clusters)
  const done = clusters.filter(
    ([, c]) => c.vuln_initial_sync_done && c.sbom_initial_sync_done,
  ).length
  const pending = clusters
    .filter(([, c]) => !c.vuln_initial_sync_done || !c.sbom_initial_sync_done)
    .map(([name]) => name)

  return (
    <div
      role="status"
      style={{
        margin: '0 0 16px',
        padding: '10px 14px',
        border: '1px solid var(--warning, #b7791f)',
        borderRadius: 6,
        background: 'var(--warning-bg, rgba(183, 121, 31, 0.08))',
        fontSize: 13,
        lineHeight: 1.5,
      }}
    >
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
    </div>
  )
}
