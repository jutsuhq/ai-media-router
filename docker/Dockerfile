# Packs the Linux binaries built by .github/workflows/release.yml (docker-bin/<arch>/ai-media-router).
# Ubuntu 24.04 matches the glibc of the GitHub runners that build them; ffmpeg makes previews.
FROM ubuntu:24.04

ARG TARGETARCH
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates ffmpeg \
    && rm -rf /var/lib/apt/lists/*
COPY docker-bin/${TARGETARCH}/ai-media-router /usr/local/bin/ai-media-router

LABEL io.modelcontextprotocol.server.name="io.github.jutsuhq/ai-media-router"
LABEL org.opencontainers.image.source="https://github.com/jutsuhq/ai-media-router"
LABEL org.opencontainers.image.description="MCP server + REST API for AI image and video generation across 40+ providers"
LABEL org.opencontainers.image.licenses="MIT"

ENV AI_MEDIA_ROUTER_HOME=/data
VOLUME /data
ENTRYPOINT ["ai-media-router"]
CMD ["mcp"]
