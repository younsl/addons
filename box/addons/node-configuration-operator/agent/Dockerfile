# Statically linked binary built via GitHub Actions cross-compilation (cargo-zigbuild)
FROM alpine:3.24 AS certs
RUN apk add --no-cache ca-certificates

FROM scratch AS runtime

LABEL org.opencontainers.image.title="node-configuration-agent" \
      org.opencontainers.image.description="Agent of node-configuration-operator that keeps the OS configuration of its node in line with NodeConfiguration custom resources" \
      org.opencontainers.image.version="0.3.1" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.base.name="scratch" \
      org.opencontainers.image.deprecated="false"

ARG TARGETARCH
COPY --from=certs /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
COPY --chmod=755 node-configuration-agent-linux-${TARGETARCH} /app/node-configuration-agent
EXPOSE 8080
USER 65532:65532
ENTRYPOINT ["/app/node-configuration-agent"]
