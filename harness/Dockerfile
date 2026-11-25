# Site-study sidecar: DeepSeek Harness in headless mode, one process per run,
# an isolated workspace per run, no host filesystem.
FROM node:24-bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl git \
    && rm -rf /var/lib/apt/lists/*

RUN npm install -g @deepseek-ai/dsh@0.2.1-rc.5

WORKDIR /app
COPY server.mjs lean.yml ./
COPY skills /home/node/.agents/skills

RUN mkdir -p /data \
    && chmod -R a+rX /app /home/node/.agents \
    && chown -R node:node /data /home/node
USER node

ENV HOME=/home/node \
    PORT=8090 \
    DATA_DIR=/data \
    DSH_HOME=/data/dsh-home \
    DSH_BIN=dsh \
    DSH_PATCH=/app/lean.yml

EXPOSE 8090
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8090/health/ready || exit 1

CMD ["node", "server.mjs"]
