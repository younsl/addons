# Statically linked binary built via GitHub Actions cross-compilation (cargo-zigbuild)
FROM alpine:3.23 AS certs
RUN apk add --no-cache ca-certificates

FROM scratch AS runtime

LABEL org.opencontainers.image.title="ec2-metadata-exporter" \
      org.opencontainers.image.description="Prometheus exporter that publishes every EC2 instance's private IP and Name tag via the DescribeInstances API" \
      org.opencontainers.image.version="0.2.0" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.base.name="scratch" \
      org.opencontainers.image.deprecated="false"

ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt

ARG TARGETARCH
COPY --from=certs /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
COPY --chmod=755 ec2-metadata-exporter-linux-${TARGETARCH} /app/ec2-metadata-exporter
EXPOSE 8080 8081
USER 65532:65532
ENTRYPOINT ["/app/ec2-metadata-exporter"]
