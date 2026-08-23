package servingcert

import (
	"crypto/rand"
	"crypto/rsa"
	"crypto/tls"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/pem"
	"io"
	"log/slog"
	"math/big"
	"os"
	"path/filepath"
	"testing"
	"time"
)

// writePair writes a self-signed keypair carrying serial and returns its paths.
func writePair(t *testing.T, dir string, serial int64) (string, string) {
	t.Helper()

	key, err := rsa.GenerateKey(rand.Reader, 2048)
	if err != nil {
		t.Fatalf("GenerateKey() error = %v", err)
	}
	template := x509.Certificate{
		SerialNumber: big.NewInt(serial),
		Subject:      pkix.Name{CommonName: "argocd-promotion-gate"},
		DNSNames:     []string{"argocd-promotion-gate"},
		NotBefore:    time.Now().Add(-time.Hour),
		NotAfter:     time.Now().Add(24 * time.Hour),
	}
	der, err := x509.CreateCertificate(rand.Reader, &template, &template, &key.PublicKey, key)
	if err != nil {
		t.Fatalf("CreateCertificate() error = %v", err)
	}

	certPath := filepath.Join(dir, "tls.crt")
	keyPath := filepath.Join(dir, "tls.key")
	certPEM := pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der})
	keyPEM := pem.EncodeToMemory(&pem.Block{Type: "RSA PRIVATE KEY", Bytes: x509.MarshalPKCS1PrivateKey(key)})
	if err := os.WriteFile(certPath, certPEM, 0o600); err != nil {
		t.Fatalf("WriteFile() error = %v", err)
	}
	if err := os.WriteFile(keyPath, keyPEM, 0o600); err != nil {
		t.Fatalf("WriteFile() error = %v", err)
	}
	return certPath, keyPath
}

// age moves both files back in time. Two writes inside one filesystem timestamp
// tick would compare equal and the reload would be missed, so the test states
// the modification times instead of hoping they differ.
func age(t *testing.T, certPath, keyPath string, d time.Duration) {
	t.Helper()
	when := time.Now().Add(-d)
	for _, path := range []string{certPath, keyPath} {
		if err := os.Chtimes(path, when, when); err != nil {
			t.Fatalf("Chtimes() error = %v", err)
		}
	}
}

func quietLogger() *slog.Logger {
	return slog.New(slog.NewTextHandler(io.Discard, nil))
}

func serialOf(t *testing.T, cert *tls.Certificate) int64 {
	t.Helper()
	if cert == nil {
		t.Fatal("certificate is nil")
	}
	leaf := cert.Leaf
	if leaf == nil {
		parsed, err := x509.ParseCertificate(cert.Certificate[0])
		if err != nil {
			t.Fatalf("ParseCertificate() error = %v", err)
		}
		leaf = parsed
	}
	return leaf.SerialNumber.Int64()
}

func TestGetCertificatePicksUpAReplacement(t *testing.T) {
	// The failure this guards is a cert-manager re-issue: the Secret changes
	// under a running pod and nothing in the pod spec moves, so a process that
	// only reads the pair once keeps serving a leaf the published caBundle no
	// longer chains to.
	dir := t.TempDir()
	certPath, keyPath := writePair(t, dir, 1)
	age(t, certPath, keyPath, time.Hour)

	reloader := NewReloader(certPath, keyPath, quietLogger())
	first, err := reloader.GetCertificate(nil)
	if err != nil {
		t.Fatalf("GetCertificate() error = %v", err)
	}
	if got := serialOf(t, first); got != 1 {
		t.Fatalf("serial = %d, want 1", got)
	}

	writePair(t, dir, 2)

	second, err := reloader.GetCertificate(nil)
	if err != nil {
		t.Fatalf("GetCertificate() after replacement error = %v", err)
	}
	if got := serialOf(t, second); got != 2 {
		t.Errorf("serial = %d, want 2 after the pair was replaced", got)
	}
}

func TestGetCertificateReusesTheLoadedPair(t *testing.T) {
	dir := t.TempDir()
	certPath, keyPath := writePair(t, dir, 7)

	reloader := NewReloader(certPath, keyPath, quietLogger())
	first, err := reloader.GetCertificate(nil)
	if err != nil {
		t.Fatalf("GetCertificate() error = %v", err)
	}
	second, err := reloader.GetCertificate(nil)
	if err != nil {
		t.Fatalf("GetCertificate() error = %v", err)
	}
	if first != second {
		t.Error("GetCertificate() reparsed an unchanged pair, want the cached pointer")
	}
}

func TestLoadFailsWhenNothingHasEverLoaded(t *testing.T) {
	dir := t.TempDir()
	reloader := NewReloader(filepath.Join(dir, "missing.crt"), filepath.Join(dir, "missing.key"), quietLogger())
	if _, err := reloader.Load(); err == nil {
		t.Fatal("Load() = nil error, want failure so startup does not come up without a certificate")
	}
	if _, err := reloader.GetCertificate(nil); err == nil {
		t.Fatal("GetCertificate() = nil error, want failure with no pair on disk")
	}
}

func TestGetCertificateKeepsTheOldPairWhenTheNewOneIsUnreadable(t *testing.T) {
	// An unreadable mount must not turn into a failed handshake: the pair
	// already in memory may still verify, a missing one never does.
	dir := t.TempDir()
	certPath, keyPath := writePair(t, dir, 3)
	age(t, certPath, keyPath, time.Hour)

	reloader := NewReloader(certPath, keyPath, quietLogger())
	if _, err := reloader.GetCertificate(nil); err != nil {
		t.Fatalf("GetCertificate() error = %v", err)
	}

	if err := os.WriteFile(certPath, []byte("not a certificate"), 0o600); err != nil {
		t.Fatalf("WriteFile() error = %v", err)
	}

	kept, err := reloader.GetCertificate(nil)
	if err != nil {
		t.Fatalf("GetCertificate() error = %v, want the previously loaded pair", err)
	}
	if got := serialOf(t, kept); got != 3 {
		t.Errorf("serial = %d, want the pair loaded before the file broke", got)
	}
}

func TestNotAfter(t *testing.T) {
	dir := t.TempDir()
	certPath, keyPath := writePair(t, dir, 5)

	reloader := NewReloader(certPath, keyPath, quietLogger())
	if got := reloader.NotAfter(); !got.IsZero() {
		t.Errorf("NotAfter() = %v before any load, want the zero time", got)
	}
	if _, err := reloader.Load(); err != nil {
		t.Fatalf("Load() error = %v", err)
	}
	if got := reloader.NotAfter(); got.IsZero() {
		t.Error("NotAfter() = zero after a load, want the leaf expiry")
	}
}
