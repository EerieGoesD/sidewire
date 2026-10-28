// ---------------------------------------------------------------------------
// Android: 15 days free, then one purchase on Google Play for lifetime use.
// Copied from Bear in Mind (bear-in-mind/src/main.js) and
// app_related/android/google_app_billing_models.txt, FREE TRIAL + ONE-TIME
// UNLOCK ON ANDROID. The trial start lives on the EERIE Worker, keyed by a hash
// of the phone's Android ID (licensing.rs says how). A Remote subscription
// unlocks everything too, so subscribers never see these pages. None of this
// runs off Android.
// ---------------------------------------------------------------------------
import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Check, Send, X } from "lucide-react";

// 15, not 7: Google's closed test runs 14 days, and a 7-day trial would lock the
// testers out halfway through it.
const TRIAL_DAYS = 15; // MUST match TRIAL_MS in licensing.rs
const DAY_MS = 86400000;
const APP_NAME = "SideWire";
// What stops when the trial ends, in the user's words: exactly what the
// backend gate (gate() in lib.rs) refuses.
const STOPS = "sending messages and files";
const BENEFITS = [
  "Notes and files between devices",
  "Rooms over your own Wi-Fi",
  "Preview images and save chats",
  "No account, no ads, no tracking",
];

// The same strings are defined in licensing.rs; keep the two identical.
const STORE_UNREACHABLE = "Can't connect to Google Play. Check your internet connection and try again.";
const NOT_STORE_BUILD = "Purchases only work in the Google Play version of SideWire.";
const TRIAL_ENDED = "Your free trial has ended. Buy SideWire to keep using it.";
const CANCEL_NOTE = "Purchase canceled.";
const PENDING_TEXT = "Your purchase is waiting for Google Play";

// Shows the screens without Play, e.g. on Windows. In the console:
//   localStorage.setItem("sidewire_dev_license", "start")   (or "trial",
//   "expired", "owned", "setup"), then reload. Never the real gate.
let devLicense = "";
try { devLicense = localStorage.getItem("sidewire_dev_license") || ""; } catch {}
const storeBuild = /Android/i.test(navigator.userAgent) || !!devLicense;

export type LicenseCard = "" | "start" | "paywall" | "offer";
type Status = { owned: boolean; pending: boolean; trialStartMs: number; nowMs: number; storeReady: boolean; setup: boolean };
type Msg = { text: string; note: boolean; at: number };

function isUserCancel(e: unknown) { return /cancel/i.test(String(e || "")); }

// Play's own messages are not written for people. Only the app's own strings
// are shown as they are.
function friendlyStoreError(e: unknown) {
  const s = String(e || "").replace(/^Error:\s*/, "");
  if ([STORE_UNREACHABLE, NOT_STORE_BUILD, TRIAL_ENDED].includes(s)) return s;
  if (/not ready|connect|network|unavailable|product not found/i.test(s)) return STORE_UNREACHABLE;
  return "The purchase did not go through. Try again.";
}

export function useLicense() {
  const [owned, setOwned] = useState(!storeBuild);
  const [pending, setPending] = useState(false);
  const [trialStartMs, setTrialStartMs] = useState(0);
  // Nothing is drawn until the first answer is in, so the start screen never
  // flashes up for someone who has already bought the app.
  const [known, setKnown] = useState(!storeBuild);
  // Play answered that the unlock does not exist yet (before it is created in
  // Play Console). The one state that leaves the app open, and only in a test
  // build: the Play build judges that answer like offline, since a re-signed
  // copy gets it too (licensing.rs, evaluate).
  const [setupPhase, setSetupPhase] = useState(false);
  // The Worker's clock minus the phone's, from the last answer (offline: the
  // latest time ever seen). The days left count on that, so setting the
  // phone's clock back buys nothing.
  const [clockOffset, setClockOffset] = useState(0);
  const [price, setPrice] = useState("");
  const [busy, setBusy] = useState(false);
  const [offerOpen, setOfferOpen] = useState(false);
  const [msg, setMsg] = useState<Msg | null>(null);
  const [, setTick] = useState(0);
  // True once Play (and the Worker) have answered this launch, so the start-up
  // retries can stop.
  const playAnswered = useRef(false);
  // The buy popup opens by itself once per launch, until the app is bought.
  const launchOfferShown = useRef(false);
  const knownRef = useRef(known);
  const ownedRef = useRef(owned);
  const priceRef = useRef("");

  const trialEndsAt = trialStartMs + TRIAL_DAYS * DAY_MS;
  const trialNow = Date.now() + clockOffset;
  let card: LicenseCard = "";
  if (storeBuild && known && !owned) {
    if (setupPhase) {
      // Open: the unlock does not exist yet.
    } else if (trialStartMs === 0) {
      card = "start";
    } else if (trialNow >= trialEndsAt) {
      card = "paywall";
    }
    if (!card && offerOpen) card = "offer";
  }
  const daysLeft = !setupPhase && trialStartMs > 0 && trialNow < trialEndsAt ? Math.max(1, Math.ceil((trialEndsAt - trialNow) / DAY_MS)) : 0;

  // The popup closes for good once a page with no way out (or nothing) takes
  // its place, so it does not come back after "Start Free Trial".
  useEffect(() => { if (offerOpen && card !== "offer") setOfferOpen(false); }, [card, offerOpen]);
  // A message belongs to the page it was said on; a page that changes by
  // itself (the trial ending) starts clean.
  useEffect(() => { setMsg(m => (m && Date.now() - m.at > 800 ? null : m)); }, [card]);
  // Said with no page up (a restore that unlocked): a short note, then gone.
  useEffect(() => {
    if (!msg || card) return;
    const id = window.setTimeout(() => setMsg(null), 2600);
    return () => window.clearTimeout(id);
  }, [msg, card]);

  const say = (text: string, note: boolean) => setMsg({ text, note, at: Date.now() });

  const refresh = useCallback(async () => {
    if (!storeBuild || devLicense) return;
    try {
      const s = await invoke<Status>("iap_status");
      playAnswered.current = !!s.storeReady || !!s.setup;
      setSetupPhase(!!s.setup);
      // Offline these are the last answers kept on the phone, judged the same
      // way as fresh ones.
      ownedRef.current = !s.setup && !!s.owned;
      setOwned(ownedRef.current);
      setPending(!!s.pending);
      setTrialStartMs(s.trialStartMs || 0);
      if (s.nowMs) setClockOffset(s.nowMs - Date.now());
      knownRef.current = true;
      setKnown(true);
      if (s.storeReady && !priceRef.current) {
        priceRef.current = await invoke<string>("iap_price").catch(() => "");
        setPrice(priceRef.current);
      }
    } catch {
      // Keep what was known; the next look tries again.
    }
    // Once per launch, the first time the answer is in: the buy popup, unless
    // the app is bought or a page that cannot be closed is already up.
    if (knownRef.current && !launchOfferShown.current) {
      launchOfferShown.current = true;
      if (!ownedRef.current) setOfferOpen(true);
    }
  }, []);

  useEffect(() => {
    if (!storeBuild) return;
    if (devLicense) {
      const o = devLicense === "owned";
      setSetupPhase(devLicense === "setup");
      setOwned(o);
      setTrialStartMs(devLicense === "trial" ? Date.now() - 2 * DAY_MS : devLicense === "expired" ? Date.now() - (TRIAL_DAYS + 1) * DAY_MS : 0);
      setPrice(devLicense === "setup" ? "" : "€2.99");
      setKnown(true);
      launchOfferShown.current = true;
      setOfferOpen(!o);
    } else {
      // Play's connection opens a moment after launch, so an early "no answer"
      // is tried again. The first answer, fresh or kept, is enough to draw.
      (async () => {
        for (const wait of [0, 2000, 6000]) {
          if (wait) await new Promise(r => setTimeout(r, wait));
          await refresh();
          if (playAnswered.current) break;
        }
      })();
    }
    // Coming back to the app (from Play's purchase sheet, or from anywhere)
    // looks again: that is where a pending purchase clears, and where a buy
    // that never answered (pending) frees its button.
    const onVisible = () => { if (document.visibilityState !== "visible") return; setBusy(false); refresh(); };
    document.addEventListener("visibilitychange", onVisible);
    // The days left, and the moment the trial ends, without waiting for a
    // restart.
    const id = window.setInterval(() => setTick(t => t + 1), 60000);
    return () => { document.removeEventListener("visibilitychange", onVisible); window.clearInterval(id); };
  }, [refresh]);

  async function startTrial() {
    if (busy) return;
    setBusy(true);
    try {
      if (devLicense) setTrialStartMs(Date.now());
      else setTrialStartMs(await invoke<number>("iap_start_trial"));
    } catch (e) { say(friendlyStoreError(e), false); }
    setBusy(false);
  }

  async function buyUnlock() {
    if (busy) return;
    setBusy(true);
    try {
      const o = devLicense ? true : await invoke<boolean>("iap_buy");
      ownedRef.current = o;
      setOwned(o);
    } catch (e) {
      if (isUserCancel(e)) say(CANCEL_NOTE, true); else say(friendlyStoreError(e), false);
    }
    setBusy(false);
  }

  async function restoreUnlock() {
    if (busy) return;
    setBusy(true);
    try {
      // An error unless Play answered, so "owned" here is Play's word, never
      // the cache's.
      const o = devLicense ? devLicense === "owned" : await invoke<boolean>("iap_restore");
      ownedRef.current = o;
      setOwned(o);
      say(o ? "Purchase restored." : "No purchase found for this Google account.", true);
    } catch (e) {
      if (isUserCancel(e)) say(CANCEL_NOTE, true); else say(STORE_UNREACHABLE, false);
    }
    setBusy(false);
  }

  const closeOffer = () => setOfferOpen(false);
  const openOffer = () => setOfferOpen(true);
  // "Buy SideWire" in the help links: only while there is something to buy and
  // no page is up.
  const canBuy = storeBuild && known && !owned && card === "";

  return { card, owned, pending, price, busy, daysLeft, msg, canBuy, refresh, startTrial, buyUnlock, restoreUnlock, closeOffer, openOffer };
}

export type License = ReturnType<typeof useLicense>;

// One full-screen page for all three. The start screen leads with "free" and
// shows how the trial runs; the price there is one line of small print, since
// nothing is being bought yet. Where something IS being bought (the ended
// trial and the offer) the price is the plan, large. Left out entirely when
// Play gave no price, never a placeholder.
export function LicenseOverlay({ lic }: { lic: License }) {
  const { card, price, busy, daysLeft, msg, pending } = lic;
  if (!card) return msg ? <div className="toast lic-toast"><Check size={16} /><span>{msg.text}</span></div> : null;

  const start = card === "start";
  const title = start
    ? `Try ${APP_NAME} free for ${TRIAL_DAYS} days`
    : card === "paywall"
      ? "Your free trial has ended"
      : daysLeft
        ? `${daysLeft} ${daysLeft === 1 ? "day" : "days"} left in your free trial`
        : `Buy ${APP_NAME}`;
  const text = start
    ? "Messages, files and local rooms included. No account, no card, no ads."
    : card === "paywall"
      ? `Unlock ${APP_NAME} to keep ${STOPS}. Your saved conversations are right where you left them.`
      : `Unlock it now and keep ${STOPS} for good.`;
  const main = start ? "Start Free Trial" : price ? `Unlock for ${price}` : `Buy ${APP_NAME}`;
  const line = msg ? msg : pending && (card === "paywall" || card === "offer") ? { text: `${PENDING_TEXT}.`, note: true } : null;

  return (
    <div className={`lic-overlay${card === "offer" ? " has-close" : ""}`} role="dialog" aria-modal="true" aria-labelledby="licTitle"
      onClick={e => { if (e.target === e.currentTarget && card === "offer") lic.closeOffer(); }}>
      <div className="lic-scroll">
        <div className="lic-inner">
          {/* The offer only: a cross, top left. The start screen and the ended trial have no way out. */}
          <div className="lic-bar">
            {card === "offer" && <button type="button" className="lic-close" onClick={lic.closeOffer} title="Not now" aria-label="Not now"><X size={20} /></button>}
          </div>
          <div className="lic-hero" aria-hidden="true"><Send size={44} /></div>
          <h3 className="lic-title" id="licTitle">{title}</h3>
          <p className="lic-text">{text}</p>
          <ul className="lic-benefits">{BENEFITS.map(b => <li key={b}>{b}</li>)}</ul>
          {start ? (
            <ol className="lic-timeline">
              <li className="now"><strong>Today</strong><span>Messages and files, free.</span></li>
              <li><strong>Day {TRIAL_DAYS}</strong><span>The free trial ends. Nothing is charged.</span></li>
              <li><strong>After that</strong><span>{price ? `${price} once` : "One payment"} to keep {STOPS}. Not a subscription.</span></li>
            </ol>
          ) : price ? (
            <div className="lic-price">
              <div className="lic-plan-words"><span className="lic-plan-name">Lifetime</span><span className="lic-plan-note">Pay once. Yours for good.</span></div>
              <strong>{price}</strong>
            </div>
          ) : null}
        </div>
      </div>
      {/* Pinned at the bottom. The message line has its own height, so a message never moves the button. */}
      <div className="lic-foot">
        <p className={`lic-msg${line?.note ? " note" : ""}`} aria-live="polite">{line?.text || ""}</p>
        <button type="button" className="lic-main" onClick={start ? lic.startTrial : lic.buyUnlock} disabled={busy}>{main}</button>
        <p className="lic-fine">{start ? "Nothing is charged automatically." : "One payment through Google Play. No subscription."}</p>
        <button type="button" className="lic-link" onClick={lic.restoreUnlock} disabled={busy}>Restore Purchases</button>
      </div>
    </div>
  );
}
