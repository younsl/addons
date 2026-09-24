# Statically linked binary built via GitHub Actions cross-compilation (cargo-zigbuild)
FROM alpine:3.23 AS certs
RUN apk add --no-cache ca-certificates

FROM scratch AS runtime

LABEL org.opencontainers.image.title="forklift" \
      org.opencontainers.image.description="Lightweight Kubernetes-native artifact repository (Maven, npm, Cargo, Go) with proxy caching and supply-chain age policy" \
      org.opencontainers.image.version="0.13.4" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.base.name="scratch" \
      org.opencontainers.image.deprecated="false"

ARG TARGETARCH
COPY --from=certs /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
COPY --chmod=755 forklift-linux-${TARGETARCH} /app/forklift
# 8080 HTTP API, UI and registry endpoints, 8081 Prometheus metrics
EXPOSE 8080 8081
USER 65532:65532
ENTRYPOINT ["/app/forklift"]
