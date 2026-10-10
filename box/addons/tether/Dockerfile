# Statically linked binary built via GitHub Actions cross-compilation (cargo-zigbuild)
FROM scratch AS runtime

LABEL org.opencontainers.image.title="tether" \
      org.opencontainers.image.description="Server that keeps dotfiles symlinked into a home directory" \
      org.opencontainers.image.version="0.1.0" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.base.name="scratch" \
      org.opencontainers.image.deprecated="false"

ARG TARGETARCH
COPY --chmod=755 tether-linux-${TARGETARCH} /app/tether
USER 65532:65532
EXPOSE 8080
ENTRYPOINT ["/app/tether"]
