# Statically linked binary built via GitHub Actions cross-compilation (cargo-zigbuild)
FROM alpine:3.24 AS certs
RUN apk add --no-cache ca-certificates

FROM scratch AS runtime

LABEL org.opencontainers.image.title="node-configuration-operator" \
      org.opencontainers.image.description="Controller that rolls NodeConfiguration custom resources out to EKS managed node group nodes" \
      org.opencontainers.image.version="0.3.1" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.base.name="scratch" \
      org.opencontainers.image.deprecated="false"

ARG TARGETARCH
COPY --from=certs /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
COPY --chmod=755 node-configuration-operator-linux-${TARGETARCH} /app/node-configuration-operator
EXPOSE 8080
USER 65532:65532
ENTRYPOINT ["/app/node-configuration-operator"]
