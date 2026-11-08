# Statically linked binary built via GitHub Actions cross-compilation (cargo-zigbuild)
FROM alpine:3.24 AS certs
# hadolint ignore=DL3018
RUN apk add --no-cache ca-certificates

FROM scratch AS runtime

LABEL org.opencontainers.image.title="ward" \
      org.opencontainers.image.description="Kubernetes-native open source antivirus and EDR: operator, manager StatefulSet, and eBPF node agent DaemonSet" \
      org.opencontainers.image.version="0.1.0" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.base.name="scratch" \
      org.opencontainers.image.deprecated="false"

# The manager pulls the signature feed from an OCI registry over TLS, and
# scratch has no trust store.
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt

ARG TARGETARCH
COPY --from=certs /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
COPY --chmod=755 ward-linux-${TARGETARCH} /app/ward
EXPOSE 8080 8081 8443
USER 65532:65532
ENTRYPOINT ["/app/ward"]
