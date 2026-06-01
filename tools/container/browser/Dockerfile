FROM mcr.microsoft.com/playwright:v1.51.0-noble

WORKDIR /app

# Copy and install dependencies. patchright is pinned to 1.51.0 to match the
# base image's bundled Chromium revision, so its postinstall reuses the browser
# already in PLAYWRIGHT_BROWSERS_PATH rather than downloading another copy.
COPY package.json /app/package.json
RUN npm install --production

# Copy IPC client + its modules
COPY ipc_client.mjs /app/ipc_client.mjs
COPY challenge.mjs /app/challenge.mjs

# Create IPC socket directory
RUN mkdir -p /ipc && chown pwuser:pwuser /ipc

# Pre-create the X11 socket directory as root. Xvfb refuses to create
# /tmp/.X11-unix when running as non-root (pwuser), which silently leaves headed
# Chromium unable to connect to the display and hang forever. Creating it here
# (root-owned, mode 1777) lets the unprivileged Xvfb reuse it cleanly.
RUN mkdir -p /tmp/.X11-unix && chmod 1777 /tmp/.X11-unix

USER pwuser

# Run headed Chromium under a virtual X display. Headed-under-Xvfb defeats the
# headless fingerprint that Cloudflare and similar anti-bot layers key on;
# xvfb-run ships with the Playwright base image. The node IPC loop is
# long-lived, so the X server stays up for the container's lifetime.
CMD ["xvfb-run", "--auto-servernum", "--server-args=-screen 0 1920x1080x24", "node", "/app/ipc_client.mjs"]
