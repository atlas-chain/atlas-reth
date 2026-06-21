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

COPY --chmod=0755 build-artifacts/arkiv-node /usr/local/bin/arkiv-node
COPY --chmod=0755 build-artifacts/arkiv-cli /usr/local/bin/arkiv-cli

RUN mkdir -p /app /home/docker \
  && chown -R docker:docker /app /home/docker
WORKDIR /app

USER docker

ENTRYPOINT ["arkiv-node"]
