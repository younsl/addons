import { useCallback, useEffect, useState } from 'react'

const STORAGE_KEY = 'trivy-collector.sidebar.collapsed'

/**
 * Read the stored preference. Site data can be unavailable (private window,
 * cleared storage, a browser configured to block it) and some contexts throw on
 * access rather than returning null, so a failure has to degrade to the default
 * rather than break the shell.
 */
function readStored(): boolean {
  try {
    return window.localStorage.getItem(STORAGE_KEY) === 'true'
  } catch {
    return false
  }
}

/**
 * Whether the sidebar is folded to its icon rail, persisted per browser.
 *
 * The preference belongs to the person looking at the page rather than to the
 * deployment, so it stays in local storage instead of travelling to the server.
 */
export function useSidebarCollapsed(): [boolean, () => void] {
  const [collapsed, setCollapsed] = useState(readStored)

  useEffect(() => {
    try {
      window.localStorage.setItem(STORAGE_KEY, String(collapsed))
    } catch {
      // The fold still works for this session; only the memory of it is lost.
    }
  }, [collapsed])

  const toggle = useCallback(() => setCollapsed((c) => !c), [])

  return [collapsed, toggle]
}
