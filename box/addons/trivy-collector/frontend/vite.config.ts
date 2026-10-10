import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'
import { defineConfig, type Plugin } from 'vite'
import react from '@vitejs/plugin-react'
import pkg from './package.json'

/** Stable path the Rust API-docs handler references. */
const SCALAR_ASSET = 'assets/scalar-standalone.js'

/**
 * Emit the Scalar standalone bundle into the build output under a fixed name.
 *
 * Scalar's documented integration is a jsdelivr script tag, which this cannot
 * use: the container is `scratch`, the cluster may have no egress, and an
 * unpinned CDN reference would let the docs UI change without a release. The
 * bundle is self-contained, so shipping it as an asset is enough.
 *
 * The name is deliberately unhashed. The page that loads it is rendered by the
 * server, which has no way to learn a content hash chosen at frontend build
 * time.
 */
function scalarStandalone(): Plugin {
  return {
    name: 'trivy-collector:scalar-standalone',
    apply: 'build',
    generateBundle() {
      // The package's exports map has no key for the standalone build, so it
      // is located relative to the main entry rather than deep-imported.
      const require = createRequire(import.meta.url)
      const dist = dirname(require.resolve('@scalar/api-reference'))
      this.emitFile({
        type: 'asset',
        fileName: SCALAR_ASSET,
        source: readFileSync(join(dist, 'browser', 'standalone.js')),
      })
    },
  }
}

export default defineConfig({
  plugins: [react(), scalarStandalone()],
  define: {
    __REACT_VERSION__: JSON.stringify(pkg.dependencies.react.replace('^', '')),
    __TYPESCRIPT_VERSION__: JSON.stringify(pkg.devDependencies.typescript.replace('~', '')),
    __VITE_VERSION__: JSON.stringify(pkg.devDependencies.vite.replace('^', '')),
    __NODE_VERSION__: JSON.stringify(process.version),
  },
  build: {
    outDir: '../static',
    emptyOutDir: true,
    // Note: emptyOutDir deletes all files in ../static/ before build.
    // The .gitignore for build output is at the trivy-collector root level.
    chunkSizeWarningLimit: 600,
    rollupOptions: {
      output: {
        // Split third-party deps into stable vendor chunks so the main bundle
        // stops tripping Vite's 500kB size warning. Grouping keeps the number
        // of chunks small while isolating the heaviest libraries.
        manualChunks: (id) => {
          if (!id.includes('node_modules')) return undefined
          if (id.includes('/react/') || id.includes('/react-dom/') || id.includes('/scheduler/')) {
            return 'vendor-react'
          }
          if (id.includes('/react-router')) return 'vendor-router'
          if (id.includes('/chart.js') || id.includes('/react-chartjs-2')) {
            return 'vendor-chart'
          }
          if (id.includes('/cytoscape')) return 'vendor-cytoscape'
          if (id.includes('/html2canvas')) return 'vendor-html2canvas'
          return 'vendor'
        },
      },
    },
  },
  server: {
    port: 5173,
    proxy: {
      '/api': 'http://localhost:3000',
      '/healthz': 'http://localhost:3000',
    },
  },
})
