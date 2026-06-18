# syntax=docker/dockerfile:1.4
# Runtime stage — binaries are pre-compiled via cross and passed via build context.
FROM registry.suse.com/bci/bci-minimal:15.7

ARG TARGETARCH
ARG BINARY=rancher-helm-mcp

COPY --from=binaries linux/${TARGETARCH}/rancher-helm-mcp /usr/local/bin/rancher-helm-mcp

USER 1001

ENTRYPOINT ["/usr/local/bin/rancher-helm-mcp"]
