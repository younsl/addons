# Statically linked binary built via GitHub Actions cross-compilation (cargo-zigbuild)
FROM alpine:3.23 AS certs
RUN apk add --no-cache ca-certificates

FROM scratch AS runtime

LABEL org.opencontainers.image.title="aws-vpn-maintenance-handler" \
      org.opencontainers.image.description="Owns AWS Site-to-Site VPN tunnel endpoint maintenance, applying it in a maintenance window after Slack approval" \
      org.opencontainers.image.version="0.1.0" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.base.name="scratch" \
      org.opencontainers.image.deprecated="false"

# The AWS SDK, the Slack Web API, and the Socket Mode WebSocket all need the
# trust store, and scratch has none. The timezone database is compiled into the
# binary (chrono-tz), so maintenanceWindow.timezone needs no /usr/share/zoneinfo.
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt

ARG TARGETARCH
COPY --from=certs /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
COPY --chmod=755 aws-vpn-maintenance-handler-linux-${TARGETARCH} /app/aws-vpn-maintenance-handler
EXPOSE 8081 9090
USER 65532:65532
ENTRYPOINT ["/app/aws-vpn-maintenance-handler"]
