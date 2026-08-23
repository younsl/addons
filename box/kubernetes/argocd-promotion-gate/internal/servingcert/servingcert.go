// Package servingcert serves the webhook's TLS keypair from disk and picks up a
// replacement without a restart.
//
// ListenAndServeTLS reads the pair once when the listener starts, so a process
// outlives the certificate it was given. Nothing corrects that on its own here:
// the pair comes from a Secret whose name never changes, so no pod spec moves
// and no rollout happens when cert-manager re-issues. cainjector publishes the
// new ca.crt into caBundle while the process still offers the previous leaf, and
// with failurePolicy Fail that is every gated sync in the cluster, invisible
// from this side because the request never reaches the handler.
package servingcert

import (
	"crypto/tls"
	"fmt"
	"log/slog"
	"os"
	"sync"
	"time"
)

// Reloader loads the keypair on demand and caches it until the files change.
type Reloader struct {
	certFile string
	keyFile  string
	logger   *slog.Logger

	mu      sync.RWMutex
	current *tls.Certificate
	certMod time.Time
	keyMod  time.Time
}

// NewReloader builds a reloader over the two PEM paths.
func NewReloader(certFile, keyFile string, logger *slog.Logger) *Reloader {
	return &Reloader{certFile: certFile, keyFile: keyFile, logger: logger}
}

// GetCertificate is the tls.Config callback. It is called per handshake, so the
// common path is a stat of two files against the modification times already
// recorded, not a parse.
func (r *Reloader) GetCertificate(*tls.ClientHelloInfo) (*tls.Certificate, error) {
	certInfo, certErr := os.Stat(r.certFile)
	keyInfo, keyErr := os.Stat(r.keyFile)
	if certErr == nil && keyErr == nil {
		r.mu.RLock()
		current := r.current
		fresh := current != nil &&
			certInfo.ModTime().Equal(r.certMod) &&
			keyInfo.ModTime().Equal(r.keyMod)
		r.mu.RUnlock()
		if fresh {
			return current, nil
		}
	}
	return r.Load()
}

// Load reads the pair and replaces the cached one.
//
// Called eagerly at startup so a broken pair still fails the process the way
// ListenAndServeTLS used to, rather than waiting for the first handshake.
func (r *Reloader) Load() (*tls.Certificate, error) {
	pair, err := tls.LoadX509KeyPair(r.certFile, r.keyFile)
	if err != nil {
		// Serving the pair already in memory beats failing the handshake: the
		// old leaf may still verify against caBundle, a missing one never can.
		r.mu.RLock()
		defer r.mu.RUnlock()
		if r.current != nil {
			r.logger.Warn("could not reload the webhook certificate, serving the one already loaded",
				"certFile", r.certFile, "error", err)
			return r.current, nil
		}
		return nil, fmt.Errorf("load webhook certificate %s: %w", r.certFile, err)
	}

	certInfo, certErr := os.Stat(r.certFile)
	keyInfo, keyErr := os.Stat(r.keyFile)

	r.mu.Lock()
	r.current = &pair
	if certErr == nil {
		r.certMod = certInfo.ModTime()
	}
	if keyErr == nil {
		r.keyMod = keyInfo.ModTime()
	}
	r.mu.Unlock()

	// The issuer is logged next to the expiry because the failure this package
	// exists for is a re-issued CA, not an expired leaf.
	if leaf := pair.Leaf; leaf != nil {
		r.logger.Info("webhook certificate loaded",
			"subject", leaf.Subject.CommonName,
			"issuer", leaf.Issuer.CommonName,
			"dnsNames", leaf.DNSNames,
			"notAfter", leaf.NotAfter.UTC().Format(time.RFC3339),
		)
	} else {
		r.logger.Info("webhook certificate loaded", "certFile", r.certFile)
	}
	return &pair, nil
}

// NotAfter is the expiry of the cached leaf, zero when none is loaded or the
// leaf could not be parsed.
func (r *Reloader) NotAfter() time.Time {
	r.mu.RLock()
	defer r.mu.RUnlock()
	if r.current == nil || r.current.Leaf == nil {
		return time.Time{}
	}
	return r.current.Leaf.NotAfter
}
