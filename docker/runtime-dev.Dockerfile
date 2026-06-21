# syntax=docker/dockerfile:1.7

ARG UBUNTU_VERSION=26.04
FROM ubuntu:${UBUNTU_VERSION}

ARG FOUNDRY_ARCH=amd64

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && update-ca-certificates \
    && groupadd --gid 714 docker \
    && useradd --uid 714 --gid docker --create-home --home-dir /home/docker --shell /usr/sbin/nologin docker \
    && rm -rf /var/lib/apt/lists/*

RUN curl -sL https://github.com/foundry-rs/foundry/releases/download/v1.7.0/foundry_v1.7.0_linux_${FOUNDRY_ARCH}.tar.gz | tar -xz \
  && mv forge cast anvil chisel /usr/local/bin/

COPY --chmod=0755 build-artifacts/arkiv-node /usr/local/bin/arkiv-node
COPY --chmod=0755 build-artifacts/arkiv-cli /usr/local/bin/arkiv-cli
COPY --chmod=0755 docker/fund-account.sh /usr/local/bin/fund-account.sh

COPY chainspec/dev.base.json /opt/arkiv/dev.base.json
COPY --chmod=0755 docker/dev-entrypoint.sh /usr/local/bin/dev-entrypoint.sh

RUN mkdir -p /app /home/docker \
  && chown -R docker:docker /app /home/docker /opt/arkiv
WORKDIR /app

USER docker

EXPOSE 8545 8546

ENTRYPOINT ["/usr/local/bin/dev-entrypoint.sh"]
