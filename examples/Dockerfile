# Build from a repository checkout with the Linux release executable saved as rift:
# docker build -f examples/Dockerfile -t rift:local .
# Ubuntu 24.04 matches the release runner's libc. Use a binary for this image's CPU.
FROM ubuntu:24.04
RUN apt-get update \
    && apt-get install --no-install-recommends -y libgcc-s1 \
    && rm -rf /var/lib/apt/lists/*
ARG RIFT_BINARY=rift
COPY --chmod=0755 ${RIFT_BINARY} /usr/local/bin/rift
# Fail the build if the supplied binary cannot execute in the runtime image.
RUN /usr/local/bin/rift --version
WORKDIR /etc/rift
USER 65532:65532
EXPOSE 25565/tcp
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/rift"]
CMD ["--config", "/etc/rift/rift.lua"]
