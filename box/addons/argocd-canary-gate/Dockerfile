# Statically linked binary built via GitHub Actions cross-compilation (cargo-zigbuild)
FROM alpine:3.23 AS certs
RUN apk add --no-cache ca-certificates

FROM scratch AS runtime

LABEL org.opencontainers.image.title="argocd-canary-gate" \
      org.opencontainers.image.description="Admission gate that blocks an Argo CD Application sync while an Argo Rollouts canary is still in progress" \
      org.opencontainers.image.version="0.1.0" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.base.name="scratch" \
      org.opencontainers.image.deprecated="false"

# The Kubernetes API is reached over TLS, so the trust store is not optional
# even though it is in-cluster.
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt

ARG TARGETARCH
COPY --from=certs /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
COPY --chmod=755 argocd-canary-gate-linux-${TARGETARCH} /app/argocd-canary-gate
EXPOSE 8443 8080
USER 65532:65532
ENTRYPOINT ["/app/argocd-canary-gate"]
