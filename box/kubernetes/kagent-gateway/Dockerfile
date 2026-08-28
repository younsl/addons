# Statically linked binary built via GitHub Actions cross-compilation (cargo-zigbuild)
FROM alpine:3.23 AS certs
RUN apk add --no-cache ca-certificates

FROM scratch AS runtime

LABEL org.opencontainers.image.title="kagent-gateway" \
      org.opencontainers.image.description="Posts Alertmanager alerts to Slack and replies in-thread with an analysis from a kagent agent over A2A" \
      org.opencontainers.image.version="0.1.0" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.base.name="scratch" \
      org.opencontainers.image.deprecated="false"

# The Slack Web API is reached over TLS, so the trust store is not optional.
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt

ARG TARGETARCH
COPY --from=certs /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
COPY --chmod=755 kagent-gateway-linux-${TARGETARCH} /app/kagent-gateway
EXPOSE 8080 8081
USER 65532:65532
ENTRYPOINT ["/app/kagent-gateway"]
