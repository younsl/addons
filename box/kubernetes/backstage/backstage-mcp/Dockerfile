# Statically linked binary built via GitHub Actions cross-compilation (cargo-zigbuild)
FROM alpine:3.23 AS certs
RUN apk add --no-cache ca-certificates

FROM scratch AS runtime

LABEL org.opencontainers.image.title="backstage-mcp" \
      org.opencontainers.image.description="Read-only MCP server exposing the Backstage catalog, search, TechDocs and in-house plugins to AI agents" \
      org.opencontainers.image.version="0.1.0" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.source="https://github.com/younsl/o" \
      org.opencontainers.image.base.name="scratch" \
      org.opencontainers.image.deprecated="false"

# Backstage may sit behind TLS, so the trust store is not optional.
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt

ARG TARGETARCH
COPY --from=certs /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
COPY --chmod=755 backstage-mcp-linux-${TARGETARCH} /app/backstage-mcp
EXPOSE 8080
USER 65532:65532
ENTRYPOINT ["/app/backstage-mcp"]
