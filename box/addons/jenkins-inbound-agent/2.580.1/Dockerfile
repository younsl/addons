# Jenkins inbound agent with sshpass, AWS CLI v2 and Amazon Corretto 11/17/21 baked in.
# Extends the upstream inbound-agent used by batch agents so
# jobs can run `aws s3 cp`, sshpass-based steps and pick a JDK (11/17/21) without
# runtime installs. The agent's own runtime JDK (Temurin 25 at /opt/java/openjdk)
# stays the default `java`; Corretto is additive for build steps.
#
# Usage:
#   docker build -t jenkins-inbound-agent:3385.vf1123fb_515da_-1-jdk25 .
#   docker run --rm -it jenkins-inbound-agent:3385.vf1123fb_515da_-1-jdk25 aws --version

ARG BASE_IMAGE=jenkins/inbound-agent
ARG BASE_TAG=3385.vf1123fb_515da_-1-jdk25
ARG AWSCLI_VERSION=2.22.35

# --- Stage 1: install AWS CLI v2 into an isolated prefix ---
FROM ${BASE_IMAGE}:${BASE_TAG} AS awscli
ARG AWSCLI_VERSION
ARG TARGETARCH
USER root
RUN set -eux; \
    arch="${TARGETARCH:-$(dpkg --print-architecture)}"; \
    case "${arch}" in \
      amd64) awscli_arch="x86_64" ;; \
      arm64) awscli_arch="aarch64" ;; \
      *) echo "unsupported arch: ${arch}" >&2; exit 1 ;; \
    esac; \
    apt-get update; \
    apt-get install -y --no-install-recommends curl unzip ca-certificates; \
    curl -fsSL "https://awscli.amazonaws.com/awscli-exe-linux-${awscli_arch}-${AWSCLI_VERSION}.zip" -o /tmp/awscliv2.zip; \
    unzip -q /tmp/awscliv2.zip -d /tmp; \
    /tmp/aws/install -i /opt/aws-cli -b /usr/local/bin; \
    rm -rf /tmp/aws /tmp/awscliv2.zip /var/lib/apt/lists/*

# --- Stage 2: final agent image ---
FROM ${BASE_IMAGE}:${BASE_TAG}

LABEL maintainer="Younsung Lee <cysl@kakao.com>" \
      org.opencontainers.image.title="jenkins-inbound-agent" \
      org.opencontainers.image.description="Jenkins inbound agent with sshpass, AWS CLI v2 and Amazon Corretto 11/17/21 baked in" \
      org.opencontainers.image.version="3385.vf1123fb_515da_-1-jdk25" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.base.name="docker.io/jenkins/inbound-agent:3385.vf1123fb_515da_-1-jdk25" \
      org.opencontainers.image.deprecated="false"

# Pin the container timezone so job timestamps/logs render in KST.
ENV TZ=Asia/Seoul

# sshpass via apt (replaces the runtime apk install done by the agent init container)
USER root
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends sshpass; \
    rm -rf /var/lib/apt/lists/*

# AWS CLI v2 from the builder stage (no curl/unzip left in the final image).
# Copy the install tree and recreate the bin symlinks here; copying the
# /usr/local/bin/aws symlink directly would dereference it and break the CLI's
# relative bundled-library lookup (libpython).
COPY --from=awscli /opt/aws-cli /opt/aws-cli
RUN set -eux; \
    ln -s /opt/aws-cli/v2/current/bin/aws /usr/local/bin/aws; \
    ln -s /opt/aws-cli/v2/current/bin/aws_completer /usr/local/bin/aws_completer

# Amazon Corretto JDK 11/17/21 for build steps (selectable via the Jenkins JDK tool).
# Installs to /usr/lib/jvm/java-{11,17,21}-amazon-corretto, matching the tool.jdk
# installations registered in the Jenkins chart values.yaml (JCasC jdk-tools).
# The agent's own runtime JDK (Temurin 25 at /opt/java/openjdk) is left untouched:
# JAVA_HOME/PATH from the base image keep `java` pointing at it, so remoting is
# unaffected; Freestyle jobs override JAVA_HOME per-build via the JDK dropdown.
# Key is stored ASCII-armored (.asc) so gnupg is not needed; wget is purged after.
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends wget ca-certificates; \
    wget -qO /usr/share/keyrings/corretto-keyring.asc https://apt.corretto.aws/corretto.key; \
    echo "deb [signed-by=/usr/share/keyrings/corretto-keyring.asc] https://apt.corretto.aws stable main" \
      > /etc/apt/sources.list.d/corretto.list; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
      java-11-amazon-corretto-jdk \
      java-17-amazon-corretto-jdk \
      java-21-amazon-corretto-jdk; \
    apt-get purge -y wget; \
    rm -rf /var/lib/apt/lists/*

# Back to the unprivileged agent user; inherit the base ENTRYPOINT (jenkins-agent)
USER jenkins

# Build-time sanity check
RUN aws --version \
 && sshpass -V \
 && /usr/lib/jvm/java-11-amazon-corretto/bin/java -version \
 && /usr/lib/jvm/java-17-amazon-corretto/bin/java -version \
 && /usr/lib/jvm/java-21-amazon-corretto/bin/java -version \
 && java -version  # agent default stays Temurin 25 (/opt/java/openjdk)
