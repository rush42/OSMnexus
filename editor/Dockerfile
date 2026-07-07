FROM node:20-bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends osmium-tool \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /repo/editor

CMD ["sh", "-c", "npm install && npm run dev"]
