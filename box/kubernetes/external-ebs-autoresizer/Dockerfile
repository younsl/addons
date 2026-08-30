# Statically linked binary built via GitHub Actions cross-compilation (cargo-zigbuild)
FROM alpine:3.23 AS certs
RUN apk add --no-cache ca-certificates

FROM scratch AS runtime

LABEL org.opencontainers.image.title="external-ebs-autoresizer" \
      org.opencontainers.image.description="Auto-expands the root filesystem (ext2/3/4 or XFS) of standalone EC2 instances via EBS ModifyVolume and SSM" \
      org.opencontainers.image.version="0.6.0" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.base.name="scratch" \
      org.opencontainers.image.deprecated="false"

# AWS, the Kubernetes API, and the optional Alertmanager, Grafana, and
# Prometheus sinks are all reached over TLS, and scratch has no trust store.
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt

ARG TARGETARCH
COPY --from=certs /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
COPY --chmod=755 external-ebs-autoresizer-linux-${TARGETARCH} /app/external-ebs-autoresizer
EXPOSE 8080 8081
USER 65532:65532
ENTRYPOINT ["/app/external-ebs-autoresizer"]
