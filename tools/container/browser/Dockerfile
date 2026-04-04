FROM mcr.microsoft.com/playwright:v1.51.0-noble

WORKDIR /app

# Copy and install dependencies (playwright-core only, browsers are in the base image)
COPY package.json /app/package.json
RUN npm install --production

# Copy IPC client
COPY ipc_client.mjs /app/ipc_client.mjs

# Create IPC socket directory
RUN mkdir -p /ipc && chown pwuser:pwuser /ipc

USER pwuser
CMD ["node", "/app/ipc_client.mjs"]
