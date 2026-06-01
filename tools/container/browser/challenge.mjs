/**
 * Anti-bot / CAPTCHA challenge detection.
 *
 * The browser tool's policy on human-verification challenges is "abort cleanly":
 * we never try to solve a CAPTCHA (doing so would defeat the very control the
 * site relies on, and is where automated browsing turns into circumventing an
 * access control). Instead we detect the challenge and return a clear, terminal
 * signal so the agent reports it and moves on instead of hanging — which also
 * keeps autonomous/cron loops from going stale waiting on something a bot must
 * not do.
 *
 * Two distinct situations, treated differently:
 *
 *   - WALL: a full-page interstitial (Cloudflare "Just a moment", DataDome, a
 *     full-page hCaptcha/Turnstile). The page content is gated behind
 *     verification, so the read itself fails. -> caller aborts the action.
 *
 *   - WIDGET: a CAPTCHA embedded in an otherwise-usable page (e.g. a reCAPTCHA
 *     on a form's submit button). The page is perfectly readable; only
 *     submission is blocked. -> caller proceeds but annotates the result so the
 *     agent knows not to attempt the (impossible) automated submit.
 */

/** Prefix marking a terminal "blocked by challenge" result. Unambiguous so the
 *  agent treats it as do-not-retry rather than transient failure. */
export const CHALLENGE_PREFIX = "BLOCKED_BY_CHALLENGE:";

/**
 * Inspect the current page for anti-bot challenges.
 *
 * @param {import('patchright').Page} page
 * @returns {Promise<{wall: string|null, widget: string|null}>}
 *   `wall` is a human-readable label when the whole page is gated (else null).
 *   `widget` is set only when a CAPTCHA widget sits on an otherwise-usable page.
 */
export async function detectChallenge(page) {
  return await page.evaluate(() => {
    const title = (document.title || "").toLowerCase();
    const bodyText = (document.body?.innerText || "").trim();
    const has = (sel) => !!document.querySelector(sel);

    // ── Full-page interstitial walls ────────────────────────────────────────
    const cloudflareWall =
      /just a moment|checking your browser|attention required|verify you are (a )?human/i.test(
        title,
      ) ||
      has("#challenge-running") ||
      has("#cf-please-wait") ||
      has("#challenge-form") ||
      has('script[src*="challenge-platform"]') ||
      typeof window._cf_chl_opt !== "undefined" ||
      has("#cf-error-details"); // Cloudflare 1020 "access denied"

    const dataDomeWall =
      typeof window.DataDome !== "undefined" ||
      has('iframe[src*="captcha-delivery.com"]');

    // ── CAPTCHA widgets (may be on a usable page) ───────────────────────────
    let widget = null;
    if (
      has('iframe[src*="recaptcha"]') ||
      has(".g-recaptcha") ||
      typeof window.grecaptcha !== "undefined"
    ) {
      widget = "reCAPTCHA";
    } else if (
      has('iframe[src*="hcaptcha.com"]') ||
      has(".h-captcha") ||
      typeof window.hcaptcha !== "undefined"
    ) {
      widget = "hCaptcha";
    } else if (
      has(".cf-turnstile") ||
      typeof window.turnstile !== "undefined" ||
      has('iframe[src*="challenges.cloudflare.com"]')
    ) {
      widget = "Cloudflare Turnstile";
    }

    // A captcha that is essentially the entire page (little other content) is a
    // wall, not a form widget.
    const captchaIsWholePage = widget !== null && bodyText.length < 200;

    let wall = null;
    if (cloudflareWall) wall = "Cloudflare challenge";
    else if (dataDomeWall) wall = "DataDome challenge";
    else if (captchaIsWholePage) wall = `${widget} challenge`;

    // When it's a wall, don't also report it as a (proceedable) widget.
    return { wall, widget: wall ? null : widget };
  });
}
