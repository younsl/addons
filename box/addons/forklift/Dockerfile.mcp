# Statically linked binary built via GitHub Actions cross-compilation (cargo-zigbuild)
FROM alpine:3.23 AS certs
RUN apk add --no-cache ca-certificates

FROM scratch AS runtime

LABEL org.opencontainers.image.title="forklift-mcp" \
      org.opencontainers.image.description="Model Context Protocol server for the forklift artifact repository management API" \
      org.opencontainers.image.version="0.3.2" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.base.name="scratch" \
      org.opencontainers.image.deprecated="false"

ARG TARGETARCH
COPY --from=certs /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
COPY --chmod=755 forklift-mcp-linux-${TARGETARCH} /app/forklift-mcp
# 8080 MCP endpoint, 8081 Prometheus metrics
EXPOSE 8080 8081
USER 65532:65532
ENTRYPOINT ["/app/forklift-mcp"]
