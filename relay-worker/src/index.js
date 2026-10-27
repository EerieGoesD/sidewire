import { DurableObject } from "cloudflare:workers";

// SideWire relay on Cloudflare: the Cloudflare version of relay/server.js.
// Each Remote room is its own Durable Object, which holds the room's sockets
// (asleep while nobody sends anything) and its history in SQLite. Messages and
// files are ciphertext; the relay cannot read them.
//
// Clients connect to  /?create=1   to open a new room (the code is picked here)
//                 or  /?room=CODE  to join one.
// After that the messages are the same as on the Fly relay.

const ROOM_CODE_CHARS = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const MAX_CONN_PER_IP = 12;            // websocket connections per IP in one room
const MAX_MSGS_PER_MIN = 240;          // messages + files per IP per minute
const MAX_FILES_PER_MIN = 10;
const MAX_MESSAGE_SIZE = 256 * 1024;   // cap a single text message payload (ciphertext)
// Cloudflare refuses any incoming WebSocket message over 32 MiB, so that is the
// file ceiling; the app keeps files to 24 MB, which comes to about 32 MB here.
const CHUNK_CHARS = 1_500_000;         // a SQLite row holds at most 2 MB
const ROOM_SWEEP_MS = 24 * 60 * 60_000; // clear a room's storage once its host is gone

// ── Google Play subscription verification (Android Remote gate) ──
// Android clients must hold an active "sidewire_remote_monthly" subscription to
// create a Remote room. Verified live against Google's Android Publisher API
// using a service account (secret GOOGLE_PLAY_SERVICE_ACCOUNT_JSON_B64).
const PLAY_PACKAGE_NAME = "com.eeriegoesd.sidewire";
const PLAY_PRODUCT_ID = "sidewire_remote_monthly";
const PLAY_VERIFY_TTL_MS = 5 * 60_000; // cache a verification result this long
const playVerifyCache = new Map(); // purchaseToken -> { activeUntilMs, checkedAtMs }
let cachedGoogleToken = null; // { token, expiresAtMs }

function log(entry) {
  // Shows in the Cloudflare dashboard: Workers > sidewire-relay > Logs.
  console.log(JSON.stringify({ ...entry, timestamp: new Date().toISOString() }));
}

function generateCode() {
  const bytes = crypto.getRandomValues(new Uint8Array(6));
  return Array.from(bytes, (b) => ROOM_CODE_CHARS[b % ROOM_CODE_CHARS.length]).join("");
}

function control(env) {
  return env.CONTROL.get(env.CONTROL.idFromName("control"));
}

// Constant-time-ish comparison so admin access can't be timed. Returns false
// whenever no secret is configured, disabling the admin endpoints entirely.
function adminAuthorized(url, secret) {
  if (!secret) return false;
  const provided = url.searchParams.get("key") || "";
  if (provided.length !== secret.length) return false;
  let diff = 0;
  for (let i = 0; i < provided.length; i++) diff |= provided.charCodeAt(i) ^ secret.charCodeAt(i);
  return diff === 0;
}

async function admin(url, env) {
  if (!adminAuthorized(url, env.ADMIN_SECRET || "")) return new Response(null, { status: 404 });
  const ip = url.searchParams.get("ip") || "";
  const c = control(env);
  if (url.pathname === "/admin/ban") {
    if (ip) { await c.ban(ip); log({ level: "warn", action: "ip_banned", clientIp: ip }); }
    return Response.json({ banned: await c.list() });
  }
  if (url.pathname === "/admin/unban") {
    await c.unban(ip);
    log({ level: "info", action: "ip_unbanned", clientIp: ip });
    return Response.json({ banned: await c.list() });
  }
  if (url.pathname === "/admin/stats") return Response.json({ banned: await c.list() });
  return new Response(null, { status: 404 });
}

function base64UrlEncode(input) {
  const bytes = typeof input === "string" ? new TextEncoder().encode(input) : input;
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replaceAll("=", "");
}

function pemToArrayBuffer(pem) {
  const base64 = pem.replace("-----BEGIN PRIVATE KEY-----", "").replace("-----END PRIVATE KEY-----", "").replace(/\s/g, "");
  const binary = atob(base64);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
  return bytes.buffer;
}

function getServiceAccount(env) {
  const b64 = env.GOOGLE_PLAY_SERVICE_ACCOUNT_JSON_B64;
  if (!b64) throw new Error("Missing GOOGLE_PLAY_SERVICE_ACCOUNT_JSON_B64");
  const raw = new TextDecoder().decode(Uint8Array.from(atob(b64), (c) => c.charCodeAt(0)));
  const parsed = JSON.parse(raw);
  if (typeof parsed.client_email !== "string" || typeof parsed.private_key !== "string") throw new Error("service account missing client_email/private_key");
  return { client_email: parsed.client_email, private_key: parsed.private_key.replaceAll("\\n", "\n") };
}

async function signJwt(sa) {
  const now = Math.floor(Date.now() / 1000);
  const header = base64UrlEncode(JSON.stringify({ alg: "RS256", typ: "JWT" }));
  const payload = base64UrlEncode(JSON.stringify({
    iss: sa.client_email, scope: "https://www.googleapis.com/auth/androidpublisher",
    aud: "https://oauth2.googleapis.com/token", iat: now, exp: now + 3600,
  }));
  const signingInput = `${header}.${payload}`;
  const key = await crypto.subtle.importKey("pkcs8", pemToArrayBuffer(sa.private_key), { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" }, false, ["sign"]);
  const sig = await crypto.subtle.sign("RSASSA-PKCS1-v1_5", key, new TextEncoder().encode(signingInput));
  return `${signingInput}.${base64UrlEncode(new Uint8Array(sig))}`;
}

async function getGoogleAccessToken(env) {
  if (cachedGoogleToken && cachedGoogleToken.expiresAtMs > Date.now() + 60_000) return cachedGoogleToken.token;
  const assertion = await signJwt(getServiceAccount(env));
  const res = await fetch("https://oauth2.googleapis.com/token", {
    method: "POST", headers: { "Content-Type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({ grant_type: "urn:ietf:params:oauth:grant-type:jwt-bearer", assertion }),
  });
  if (!res.ok) throw new Error("Google OAuth token request failed: " + (await res.text()));
  const data = await res.json();
  cachedGoogleToken = { token: data.access_token, expiresAtMs: Date.now() + data.expires_in * 1000 };
  return cachedGoogleToken.token;
}

// Returns true if the purchase token is an active SideWire subscription.
async function verifyPlayToken(env, purchaseToken) {
  if (!purchaseToken) return false;
  const cached = playVerifyCache.get(purchaseToken);
  if (cached && Date.now() - cached.checkedAtMs < PLAY_VERIFY_TTL_MS) return cached.activeUntilMs > Date.now();
  const accessToken = await getGoogleAccessToken(env);
  const url = `https://androidpublisher.googleapis.com/androidpublisher/v3/applications/${PLAY_PACKAGE_NAME}/purchases/subscriptionsv2/tokens/${encodeURIComponent(purchaseToken)}`;
  const res = await fetch(url, { headers: { Authorization: `Bearer ${accessToken}`, Accept: "application/json" } });
  if (!res.ok) { console.error("Play verify failed:", await res.text()); return false; }
  const purchase = await res.json();
  const lineItem = (purchase.lineItems || []).find((i) => i.productId === PLAY_PRODUCT_ID) || null;
  const expiryMs = lineItem?.expiryTime ? Date.parse(lineItem.expiryTime) : Number.NaN;
  const accessStates = new Set(["SUBSCRIPTION_STATE_ACTIVE", "SUBSCRIPTION_STATE_IN_GRACE_PERIOD", "SUBSCRIPTION_STATE_CANCELED"]);
  const active = Boolean(lineItem) && Number.isFinite(expiryMs) && expiryMs > Date.now() && accessStates.has(purchase.subscriptionState || "");
  playVerifyCache.set(purchaseToken, { activeUntilMs: active ? expiryMs : 0, checkedAtMs: Date.now() });
  return active;
}

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (url.pathname === "/health") return Response.json({ status: "ok" });
    if (url.pathname.startsWith("/admin/")) return admin(url, env);
    if ((request.headers.get("Upgrade") || "").toLowerCase() !== "websocket") return new Response(null, { status: 404 });

    const ip = request.headers.get("CF-Connecting-IP") || "unknown";
    if (await control(env).isBanned(ip)) return new Response("banned", { status: 403 });

    const create = url.searchParams.has("create");
    const code = create
      ? generateCode()
      : (url.searchParams.get("room") || "").toUpperCase().replace(/[^A-Z0-9]/g, "").slice(0, 16) || "NONE";
    const headers = new Headers(request.headers);
    headers.set("X-Room-Code", code);
    headers.set("X-Client-Ip", ip);
    headers.set("X-Create", create ? "1" : "0");
    return env.ROOMS.get(env.ROOMS.idFromName(code)).fetch(new Request(request, { headers }));
  },
};

// The IP ban list, shared by every room.
export class Control extends DurableObject {
  constructor(ctx, env) {
    super(ctx, env);
    ctx.storage.sql.exec("CREATE TABLE IF NOT EXISTS banned (ip TEXT PRIMARY KEY)");
  }
  isBanned(ip) { return this.ctx.storage.sql.exec("SELECT 1 FROM banned WHERE ip = ?", ip).toArray().length > 0; }
  ban(ip) { this.ctx.storage.sql.exec("INSERT OR IGNORE INTO banned (ip) VALUES (?)", ip); }
  unban(ip) { this.ctx.storage.sql.exec("DELETE FROM banned WHERE ip = ?", ip); }
  list() { return this.ctx.storage.sql.exec("SELECT ip FROM banned").toArray().map((r) => r.ip); }
}

// One Remote room. What each socket is (its name, host or not, in the room or
// not) lives in the socket's attachment, so it survives the room going to sleep.
export class Room extends DurableObject {
  constructor(ctx, env) {
    super(ctx, env);
    this.sql = ctx.storage.sql;
    this.rates = new Map(); // ip -> { msgs, files, resetAt }; starts over when the room sleeps
  }

  ensureTables() {
    this.sql.exec("CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT NOT NULL)");
    this.sql.exec("CREATE TABLE IF NOT EXISTS history (seq INTEGER NOT NULL, part INTEGER NOT NULL, data TEXT NOT NULL, PRIMARY KEY (seq, part))");
  }

  getRoom() {
    this.ensureTables();
    const row = this.sql.exec("SELECT v FROM meta WHERE k = 'room'").toArray()[0];
    return row ? JSON.parse(row.v) : null;
  }

  setRoom(room) {
    this.ensureTables();
    this.sql.exec("INSERT OR REPLACE INTO meta (k, v) VALUES ('room', ?)", JSON.stringify(room));
  }

  // History is kept as the exact text each client was sent, split over rows.
  store(text) {
    this.ensureTables();
    const seq = this.sql.exec("SELECT COALESCE(MAX(seq), 0) + 1 AS n FROM history").one().n;
    for (let part = 0, i = 0; i < text.length; part++, i += CHUNK_CHARS) {
      this.sql.exec("INSERT INTO history (seq, part, data) VALUES (?, ?, ?)", seq, part, text.slice(i, i + CHUNK_CHARS));
    }
  }

  // Sends the history to a joiner one item at a time, in order, so a big file
  // never sits in memory next to the rest.
  replay(ws) {
    let seq = null;
    let parts = [];
    for (const row of this.sql.exec("SELECT seq, data FROM history ORDER BY seq, part")) {
      if (seq !== null && row.seq !== seq) { ws.send(parts.join("")); parts = []; }
      seq = row.seq;
      parts.push(row.data);
    }
    if (parts.length) ws.send(parts.join(""));
  }

  broadcast(senderId, text) {
    for (const s of this.ctx.getWebSockets()) {
      const a = s.deserializeAttachment();
      if (!a || !a.inRoom || a.id === senderId) continue;
      try { s.send(text); } catch {}
    }
  }

  rate(ip, kind, max) {
    const now = Date.now();
    let e = this.rates.get(ip);
    if (!e || now > e.resetAt) { e = { msgs: 0, files: 0, resetAt: now + 60_000 }; this.rates.set(ip, e); }
    e[kind]++;
    return e[kind] <= max;
  }

  error(ws, message) { try { ws.send(JSON.stringify({ type: "error", message })); } catch {} }

  async fetch(request) {
    const ip = request.headers.get("X-Client-Ip") || "unknown";
    const sameIp = this.ctx.getWebSockets().filter((s) => s.deserializeAttachment()?.ip === ip).length;
    const [client, server] = Object.values(new WebSocketPair());
    this.ctx.acceptWebSocket(server);
    server.serializeAttachment({
      id: crypto.randomUUID(),
      code: request.headers.get("X-Room-Code") || "",
      create: request.headers.get("X-Create") === "1",
      ip, deviceName: "unknown", isHost: false, inRoom: false,
    });
    if (sameIp >= MAX_CONN_PER_IP) server.close(4029, "too many connections");
    return new Response(null, { status: 101, webSocket: client });
  }

  async webSocketMessage(ws, raw) {
    if (typeof raw !== "string") return;
    const att = ws.deserializeAttachment() || {};
    try {
      // A file is one big message. It is stored and forwarded as the text it came
      // in, without being parsed, so the room holds as few copies as possible.
      if (raw.startsWith('{"type":"file"')) { this.onFile(ws, att, raw); return; }
      const msg = JSON.parse(raw);
      const { type } = msg;

      if (type === "create_room") {
        if (!att.create || this.getRoom()) { this.error(ws, "Could not create a room. Please try again."); return; }
        // Android clients must hold an active subscription to host a Remote room.
        // Desktop/other clients are not gated.
        if (msg.client === "android") {
          let entitled = false;
          try { entitled = await verifyPlayToken(this.env, msg.purchaseToken); } catch (e) { log({ level: "error", action: "play_verify_error", clientIp: att.ip, error: e.message }); }
          if (!entitled) { this.error(ws, "subscription_required"); return; }
        }
        att.deviceName = msg.deviceName || "Host";
        att.isHost = true;
        att.inRoom = true;
        ws.serializeAttachment(att);
        // authHash is set by a follow-up `set_auth`. Until then the room is
        // `pending` and cannot be joined.
        this.setRoom({ authHash: null, pending: true });
        await this.ctx.storage.setAlarm(Date.now() + ROOM_SWEEP_MS);
        ws.send(JSON.stringify({ type: "room_created", roomCode: att.code }));
        log({ level: "info", action: "room_created", roomCode: att.code, deviceName: att.deviceName, clientIp: att.ip });
        return;
      }

      if (type === "set_auth") {
        const room = this.getRoom();
        if (!room || !att.isHost || !att.inRoom) return;
        if (typeof msg.authHash !== "string" || msg.authHash.length < 16) { this.error(ws, "Invalid room password"); return; }
        room.authHash = msg.authHash;
        room.pending = false;
        this.setRoom(room);
        return;
      }

      if (type === "join_room") {
        const room = this.getRoom();
        if (!room) { this.error(ws, "Room not found"); return; }
        if (room.pending || !room.authHash) { this.error(ws, "Room not ready"); return; }
        // The relay never sees the password, only this derived join hash.
        if (msg.authHash !== room.authHash) { this.error(ws, "Wrong password"); return; }
        att.deviceName = msg.deviceName || "Joiner";
        att.isHost = false;
        att.inRoom = true;
        ws.serializeAttachment(att);
        // The history follows as ordinary message and file events.
        ws.send(JSON.stringify({ type: "room_joined", roomCode: att.code, messages: [] }));
        this.replay(ws);
        this.broadcast(att.id, JSON.stringify({ type: "user_joined", deviceName: att.deviceName }));
        log({ level: "info", action: "user_joined", roomCode: att.code, deviceName: att.deviceName, clientIp: att.ip });
        return;
      }

      if (type === "message") {
        if (!att.inRoom || !this.getRoom()) return;
        if (typeof msg.payload !== "string") return;
        if (msg.payload.length > MAX_MESSAGE_SIZE) { this.error(ws, "Message too large"); return; }
        if (!this.rate(att.ip, "msgs", MAX_MSGS_PER_MIN)) { this.error(ws, "Too many messages. Please slow down."); return; }
        // payload is opaque ciphertext; the relay cannot read it.
        const text = JSON.stringify({ type: "message", sender: "user", deviceName: att.deviceName, payload: msg.payload, createdAt: Date.now() });
        this.store(text);
        this.broadcast(att.id, text);
        return;
      }

      if (type === "file") { this.onFile(ws, att, raw); return; }

      if (type === "leave") { await this.leave(ws, att, "leave"); return; }
    } catch (err) {
      log({ level: "error", action: "parse_error", clientIp: att.ip, error: err.message });
    }
  }

  onFile(ws, att, raw) {
    if (!att.inRoom || !this.getRoom()) { this.error(ws, "Room not found"); return; }
    if (!this.rate(att.ip, "msgs", MAX_MSGS_PER_MIN)) { this.error(ws, "Too many messages. Please slow down."); return; }
    if (!this.rate(att.ip, "files", MAX_FILES_PER_MIN)) {
      log({ level: "warn", action: "rate_limit_hit", clientIp: att.ip, deviceName: att.deviceName, roomCode: att.code });
      this.error(ws, "Too many files. Please wait.");
      return;
    }
    // File name and contents are inside the encrypted payload; the relay only
    // sees the raw size for abuse control.
    const fileSize = Number(/"fileSize":(\d+)/.exec(raw.slice(-80))?.[1] || 0);
    log({ level: "info", action: "file_sent", roomCode: att.code, deviceName: att.deviceName, clientIp: att.ip, fileSize });
    this.store(raw);
    this.broadcast(att.id, raw);
  }

  async leave(ws, att, reason) {
    if (!att.inRoom) return;
    att.inRoom = false;
    try { ws.serializeAttachment(att); } catch {}
    if (att.isHost) {
      this.broadcast(att.id, JSON.stringify({ type: "room_closed" }));
      for (const s of this.ctx.getWebSockets()) {
        const a = s.deserializeAttachment();
        if (a?.inRoom) { a.inRoom = false; try { s.serializeAttachment(a); } catch {} }
      }
      await this.ctx.storage.deleteAlarm();
      await this.ctx.storage.deleteAll();
      log({ level: "info", action: "room_closed", roomCode: att.code, deviceName: att.deviceName, clientIp: att.ip, reason });
    } else {
      this.broadcast(att.id, JSON.stringify({ type: "user_left", deviceName: att.deviceName }));
    }
  }

  async webSocketClose(ws) { await this.leave(ws, ws.deserializeAttachment() || {}, "disconnect"); }

  async webSocketError(ws) { await this.leave(ws, ws.deserializeAttachment() || {}, "disconnect"); }

  // Safety net: a room whose host is gone without a close event still gets cleared.
  async alarm() {
    const hostHere = this.ctx.getWebSockets().some((s) => { const a = s.deserializeAttachment(); return a?.isHost && a?.inRoom; });
    if (hostHere) await this.ctx.storage.setAlarm(Date.now() + ROOM_SWEEP_MS);
    else await this.ctx.storage.deleteAll();
  }
}
