/**
 * Cherub Browser IPC Client
 *
 * Implements the Cherub container IPC protocol (length-prefixed JSON over Unix socket)
 * and dispatches browser actions to Playwright.
 *
 * Protocol:
 *   [4 bytes big-endian length] [JSON payload]
 *
 * Messages received (RuntimeMessage):
 *   - Execute { id, params, context? }
 *   - Shutdown
 *
 * Messages sent (ToolMessage):
 *   - Registration { name, description, schema }
 *   - Result { id, output?, error?, images? }
 *   - Log { level, message }
 */

import net from "node:net";
import fs from "node:fs";
import os from "node:os";
// patchright is a drop-in Playwright fork that patches the automation tells
// Cloudflare/DataDome fingerprint: the CDP `Runtime.enable` leak, the
// `navigator.webdriver` flag, the console.enable leak, and shadow-DOM
// injection. Pinned to the same minor as the base image's bundled Chromium
// (Dockerfile: mcr.microsoft.com/playwright:v1.51.0-noble) so no extra browser
// download is needed.
import { chromium } from "patchright";
import { detectChallenge, CHALLENGE_PREFIX } from "./challenge.mjs";

const SOCKET_PATH = process.env.CHERUB_IPC_SOCKET || "/ipc/tool.sock";
const MAX_TEXT_LENGTH = 16 * 1024; // 16 KB text output limit

// Persistent profile directory. Keeping a single profile for the container's
// lifetime lets a `cf_clearance` cookie (granted after a Cloudflare challenge
// passes) survive across multiple browser actions in a session instead of
// re-challenging on every call.
const PROFILE_DIR =
  process.env.CHERUB_BROWSER_PROFILE || `${os.tmpdir()}/cherub-browser-profile`;

// ─── IPC Transport ──────────────────────────────────────────────────────────

class IpcTransport {
  constructor(socket) {
    this.socket = socket;
    this.buffer = Buffer.alloc(0);
    this.pendingResolve = null;

    socket.on("data", (chunk) => {
      this.buffer = Buffer.concat([this.buffer, chunk]);
      this._tryParse();
    });
  }

  send(msg) {
    const payload = Buffer.from(JSON.stringify(msg));
    const header = Buffer.alloc(4);
    header.writeUInt32BE(payload.length, 0);
    this.socket.write(Buffer.concat([header, payload]));
  }

  recv() {
    return new Promise((resolve) => {
      this.pendingResolve = resolve;
      this._tryParse();
    });
  }

  _tryParse() {
    if (!this.pendingResolve) return;
    if (this.buffer.length < 4) return;
    const len = this.buffer.readUInt32BE(0);
    if (this.buffer.length < 4 + len) return;
    const json = this.buffer.subarray(4, 4 + len).toString("utf-8");
    this.buffer = this.buffer.subarray(4 + len);
    const resolve = this.pendingResolve;
    this.pendingResolve = null;
    resolve(JSON.parse(json));
  }
}

// ─── Browser State ──────────────────────────────────────────────────────────

let context = null;
let page = null;

async function ensureBrowser() {
  if (!context) {
    fs.mkdirSync(PROFILE_DIR, { recursive: true });
    // launchPersistentContext (not launch + newContext) is patchright's
    // recommended entry point — a real on-disk profile looks far less
    // synthetic than an ephemeral incognito context.
    //
    // headless:false runs a genuine headed Chromium; the container's CMD wraps
    // node in `xvfb-run`, providing a virtual X display. Headed Chromium under
    // Xvfb clears Cloudflare "Managed Challenge" interstitials that headless
    // signatures trip. viewport:null makes the page viewport track the real
    // window size (natural) instead of a fixed headless-style rectangle.
    //
    // No userAgent override: a spoofed UA that disagrees with the real platform
    // is itself a tell. We let Chromium report its genuine (headed) UA, which
    // patchright keeps internally consistent. --no-sandbox is required inside
    // the container; it is a weak signal compared to the leaks patchright closes.
    context = await chromium.launchPersistentContext(PROFILE_DIR, {
      headless: false,
      viewport: null,
      // --window-size matches the Xvfb screen (Dockerfile: 1920x1080) so the
      // window — and thus the viewport, with viewport:null — is a natural full
      // size rather than Chromium's tiny default.
      args: [
        "--no-sandbox",
        "--disable-setuid-sandbox",
        "--window-size=1920,1080",
      ],
    });
    page = context.pages()[0] || (await context.newPage());
  }
  return page;
}

function truncate(text, maxLen = MAX_TEXT_LENGTH) {
  if (text.length <= maxLen) return text;
  return text.slice(0, maxLen) + `\n... [truncated, ${text.length} total chars]`;
}

// ─── Action Handlers ────────────────────────────────────────────────────────

async function handleBrowse(params) {
  const url = params.url;
  if (!url) throw new Error("browse action requires 'url' parameter");

  const p = await ensureBrowser();
  const timeout = params.timeout || 30000;
  await p.goto(url, { waitUntil: "domcontentloaded", timeout });

  // Wait a bit for JS rendering
  await p.waitForTimeout(1000);

  const title = await p.title();
  const text = await p.evaluate(() => document.body?.innerText || "");
  const currentUrl = p.url();
  const challenge = await detectChallenge(p);

  // Take an automatic screenshot
  const screenshotBuf = await p.screenshot({ type: "png", fullPage: false });
  const screenshotBase64 = screenshotBuf.toString("base64");
  const images = [{ media_type: "image/png", data: screenshotBase64 }];

  // Full-page wall: the content is gated behind human verification. Abort the
  // read with a terminal signal — solving it is off-limits, and retrying or
  // waiting would just hang an autonomous loop.
  if (challenge.wall) {
    return {
      output:
        `${CHALLENGE_PREFIX} ${challenge.wall} at ${currentUrl}. This page is ` +
        `gated by a human-verification challenge (CAPTCHA) and cannot be ` +
        `accessed automatically. Do not retry — report this to the user or move on.`,
      images,
    };
  }

  let output = truncate(`Page: ${title}\nURL: ${currentUrl}\n\n${text}`);

  // Widget on an otherwise-usable page: the read succeeded, but warn that any
  // form submission gated by this CAPTCHA cannot be completed automatically.
  if (challenge.widget) {
    output =
      `Note: this page contains a ${challenge.widget} CAPTCHA — form ` +
      `submission here requires human verification and cannot be completed ` +
      `automatically.\n\n${output}`;
  }

  return { output, images };
}

async function handleClick(params) {
  if (!params.selector) throw new Error("click requires 'selector' parameter");
  const p = await ensureBrowser();
  const timeout = params.timeout || 30000;
  await p.click(params.selector, { timeout });
  // Brief wait for any navigation/rendering triggered by the click
  await p.waitForTimeout(500);
  const url = p.url();

  // A click (e.g. a form submit) can trigger a challenge wall. Surface it
  // cleanly rather than letting subsequent actions spin against a gated page.
  const challenge = await detectChallenge(p);
  if (challenge.wall) {
    return {
      output:
        `${CHALLENGE_PREFIX} ${challenge.wall} at ${url} after clicking ` +
        `"${params.selector}". A human-verification challenge appeared and ` +
        `cannot be solved automatically. Do not retry.`,
    };
  }

  return { output: `Clicked "${params.selector}" on ${url}` };
}

async function handleFill(params) {
  if (!params.selector) throw new Error("fill requires 'selector' parameter");
  if (params.value === undefined)
    throw new Error("fill requires 'value' parameter");
  const p = await ensureBrowser();
  const timeout = params.timeout || 30000;
  await p.fill(params.selector, params.value, { timeout });
  return {
    output: `Filled "${params.selector}" with "${params.value}"`,
  };
}

async function handleSelect(params) {
  if (!params.selector) throw new Error("select requires 'selector' parameter");
  if (params.value === undefined)
    throw new Error("select requires 'value' parameter");
  const p = await ensureBrowser();
  const timeout = params.timeout || 30000;
  const selected = await p.selectOption(params.selector, params.value, {
    timeout,
  });
  return { output: `Selected "${selected}" in "${params.selector}"` };
}

async function handleScreenshot(_params) {
  const p = await ensureBrowser();
  const title = await p.title();
  const url = p.url();
  const screenshotBuf = await p.screenshot({ type: "png", fullPage: false });
  const screenshotBase64 = screenshotBuf.toString("base64");
  return {
    output: `Screenshot of "${title}" (${url})`,
    images: [{ media_type: "image/png", data: screenshotBase64 }],
  };
}

async function handleEvaluate(params) {
  if (!params.script) throw new Error("evaluate requires 'script' parameter");
  const p = await ensureBrowser();
  const result = await p.evaluate(params.script);
  const output =
    typeof result === "string" ? result : JSON.stringify(result, null, 2);
  return { output: truncate(output) };
}

async function handleWaitFor(params) {
  if (!params.selector)
    throw new Error("wait_for requires 'selector' parameter");
  const p = await ensureBrowser();
  const timeout = params.timeout || 30000;
  await p.waitForSelector(params.selector, { timeout });
  return { output: `Element "${params.selector}" is now visible` };
}

async function handleGetText(params) {
  const p = await ensureBrowser();
  let text;
  if (params.selector) {
    const el = await p.$(params.selector);
    if (!el) throw new Error(`Selector "${params.selector}" not found`);
    text = await el.innerText();
  } else {
    text = await p.evaluate(() => document.body?.innerText || "");
  }
  return { output: truncate(text) };
}

async function handleGetUrl(_params) {
  const p = await ensureBrowser();
  return { output: p.url() };
}

async function handleScroll(params) {
  const p = await ensureBrowser();
  const direction = params.direction === "up" ? -1 : 1;
  const amount = params.amount || 500;
  const scrollY = direction * amount;
  await p.evaluate((y) => window.scrollBy(0, y), scrollY);
  const pos = await p.evaluate(() => ({
    y: window.scrollY,
    height: document.documentElement.scrollHeight,
  }));
  return {
    output: `Scrolled ${direction > 0 ? "down" : "up"} ${amount}px. Position: ${pos.y}/${pos.height}`,
  };
}

const ACTION_HANDLERS = {
  browse: handleBrowse,
  click: handleClick,
  fill: handleFill,
  select: handleSelect,
  screenshot: handleScreenshot,
  evaluate: handleEvaluate,
  wait_for: handleWaitFor,
  get_text: handleGetText,
  get_url: handleGetUrl,
  scroll: handleScroll,
};

// ─── Main Loop ──────────────────────────────────────────────────────────────

async function main() {
  const socket = net.createConnection(SOCKET_PATH);

  await new Promise((resolve, reject) => {
    socket.on("connect", resolve);
    socket.on("error", reject);
  });

  const transport = new IpcTransport(socket);

  // Send Registration
  transport.send({
    type: "registration",
    name: "browser",
    description:
      "Browse websites using Playwright + Chromium. Navigates JS-heavy pages, fills forms, takes screenshots.",
    schema: {
      type: "object",
      properties: {
        action: {
          type: "string",
          enum: [
            "browse",
            "click",
            "fill",
            "select",
            "screenshot",
            "evaluate",
            "wait_for",
            "get_text",
            "get_url",
            "scroll",
          ],
        },
        url: { type: "string" },
        selector: { type: "string" },
        value: { type: "string" },
        script: { type: "string" },
        timeout: { type: "integer" },
        direction: { type: "string", enum: ["up", "down"] },
        amount: { type: "integer" },
      },
      required: ["action"],
    },
  });

  transport.send({
    type: "log",
    level: "info",
    message: "Browser IPC client connected",
  });

  // Main message loop
  while (true) {
    const msg = await transport.recv();

    if (msg.type === "shutdown") {
      transport.send({
        type: "log",
        level: "info",
        message: "Shutting down browser",
      });
      if (context) {
        await context.close().catch(() => {});
      }
      process.exit(0);
    }

    if (msg.type === "execute") {
      const { id, params } = msg;
      const action = params?.action;

      try {
        const handler = ACTION_HANDLERS[action];
        if (!handler) {
          throw new Error(`Unknown browser action: "${action}"`);
        }

        const result = await handler(params);
        const response = {
          type: "result",
          id,
          output: result.output || null,
          error: null,
        };
        if (result.images && result.images.length > 0) {
          response.images = result.images;
        }
        transport.send(response);
      } catch (err) {
        transport.send({
          type: "result",
          id,
          output: null,
          error: err.message || String(err),
        });
      }
    }
  }
}

main().catch((err) => {
  console.error("Browser IPC client fatal error:", err);
  process.exit(1);
});
