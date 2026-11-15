# Build from the extracted Linux release archive (or a checkout with the executable saved as rift):
# docker build -f examples/Dockerfile -t rift:local .
# The glibc release binaries need glibc 2.35 or newer; the -musl ones run on any base.
# Use a binary for this image's CPU.
FROM ubuntu:24.04
RUN apt-get update \
    && apt-get install --no-install-recommends -y libgcc-s1 \
    && rm -rf /var/lib/apt/lists/*
ARG RIFT_BINARY=rift
COPY --chmod=0755 ${RIFT_BINARY} /usr/local/bin/rift
# Fail the build if the supplied binary cannot execute in the runtime image.
RUN /usr/local/bin/rift --version
# Docker copies this directory's ownership/mode into a new state volume.
RUN install -d -o 65532 -g 65532 -m 0700 /var/lib/rift
WORKDIR /etc/rift
USER 65532:65532
EXPOSE 25565/tcp
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/rift"]
CMD ["--config", "/etc/rift/rift.lua"]
