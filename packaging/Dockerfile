# Runtime image for an INFERENCE-ONLY kmplify-node.
#
#   docker run -d --name kmplify-node --network host \
#     -v kmplify-node-data:/data \
#     -e PROVIDER_COUNTRY=DE \
#     ghcr.io/kmplify/kmplify-node
#
# --network host is the practical default on Linux: the node's job is to
# reach the Ollama already running on this machine, and the default
# OLLAMA_BASE (http://127.0.0.1:11434) then just works. Elsewhere, set
# OLLAMA_BASE=http://host.docker.internal:11434.
#
# Scope, honestly: this image serves MODELS. It cannot host container
# sessions, because that requires driving the host's Docker daemon and
# seeing its GPU, and a worker inside a container has neither. A session
# host runs the binary on the host under systemd (scripts/install.sh
# --service). The worker detects the absence of docker and advertises
# itself inference-only, so running this image never promises the fabric
# something it cannot deliver.
#
# CI builds this per-arch by copying the already-built static musl binary
# into the context at dist/<arch>/kmplify-node. To build one locally:
#
#   docker build -f packaging/Dockerfile.node-build -o dist-node .
#   mkdir -p dist/$(docker version -f '{{.Server.Arch}}')
#   cp dist-node/kmplify-node dist/$(docker version -f '{{.Server.Arch}}')/
#   docker build -f packaging/Dockerfile -t kmplify-node .
#
# alpine rather than scratch: the binary is fully static and needs nothing,
# but a shell inside the image is what lets an operator debug a node that
# cannot reach its model server, which is the single most common support
# case a distributed fleet generates.
FROM alpine:3.21
ARG TARGETARCH
COPY dist/${TARGETARCH}/kmplify-node /usr/local/bin/kmplify-node

# Anonymous node identity lives here; mount a volume to keep it across
# container replacements, or the node re-registers as a new peer each time.
ENV KMPLIFY_NODE_DIR=/data
RUN addgroup -S kmplify && adduser -S -G kmplify -u 10001 kmplify \
 && mkdir -p /data && chown kmplify:kmplify /data
USER kmplify
VOLUME /data

# `docker run image check` runs the preflight instead of joining.
ENTRYPOINT ["/usr/local/bin/kmplify-node"]
