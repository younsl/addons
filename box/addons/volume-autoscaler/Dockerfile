# Statically linked binary built via GitHub Actions cross-compilation (cargo-zigbuild)
FROM alpine:3.24 AS certs
RUN apk add --no-cache ca-certificates

FROM scratch AS runtime

LABEL org.opencontainers.image.title="volume-autoscaler" \
      org.opencontainers.image.description="Grows EC2 root volumes and Kubernetes PersistentVolumeClaims when usage crosses a threshold" \
      org.opencontainers.image.version="0.1.0" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.base.name="scratch" \
      org.opencontainers.image.deprecated="false"

# AWS, the Kubernetes API, and the optional Alertmanager, Grafana, and
# Prometheus sinks are all reached over TLS, and scratch has no trust store.
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt

ARG TARGETARCH
COPY --from=certs /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
COPY --chmod=755 volume-autoscaler-linux-${TARGETARCH} /app/volume-autoscaler
EXPOSE 8080 8081
USER 65532:65532
ENTRYPOINT ["/app/volume-autoscaler"]
