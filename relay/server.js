import { createServer } from "http";
import { WebSocketServer } from "ws";

const PORT = process.env.PORT || 8080;
const MAX_ROOMS = 100;
// Limit is applied to the base64 payload length (encrypted content), which is
// roughly 4/3 of the raw file size. maxPayload below must stay above this.
const MAX_FILE_SIZE = 140 * 1024 * 1024;
const MAX_WS_PAYLOAD = 160 * 1024 * 1024;
const MAX_FILES_PER_MIN = 10;
const MAX_CONN_PER_IP = 12;            // concurrent websocket connections per IP
const MAX_MSGS_PER_MIN = 240;          // messages + files per IP per minute
const MAX_MESSAGE_SIZE = 256 * 1024;   // cap a single text message payload (ciphertext)
// Secret for the /admin endpoints. Set with: fly secrets set ADMIN_SECRET=...
// If unset, the admin endpoints are disabled entirely (return 404).
const ADMIN_SECRET = process.env.ADMIN_SECRET || "";

// ── Google Play subscription verification (Android Remote gate) ──
// Android clients must hold an active "sidewire_remote_monthly" subscription to
// create a Remote room. Verified live against Google's Android Publisher API
// using a service account (Fly secret GOOGLE_PLAY_SERVICE_ACCOUNT_JSON_B64).
const PLAY_PACKAGE_NAME = "com.eeriegoesd.sidewire";
const PLAY_PRODUCT_ID = "sidewire_remote_monthly";
const PLAY_VERIFY_TTL_MS = 5 * 60_000; // cache a verification result this long
const playVerifyCache = new Map(); // purchaseToken -> { activeUntilMs, checkedAtMs }
let cachedGoogleToken = null; // { token, expiresAtMs }

const rooms = new Map();
const ipFileCounts = new Map();
const ipMsgCounts = new Map();
const ipConnections = new Map();
const bannedIps = new Set();
const stats = { totalFiles: 0, totalMessages: 0, totalBytesRelayed: 0, roomsCreated: 0 };
const activityLog = [];
const MAX_LOG = 10000;

function log(entry) {
  entry.timestamp = new Date().toISOString();
  activityLog.push(entry);
  if (activityLog.length > MAX_LOG) activityLog.shift();
  // Also writes to Fly.io logs (retained for 7 days)
  console.log(JSON.stringify(entry));
}

function generateCode() {
  const chars = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
  let code;
  do { code = Array.from({ length: 6 }, () => chars[Math.floor(Math.random() * chars.length)]).join(""); } while (rooms.has(code));
  return code;
}

function broadcast(room, sender, data) {
  const msg = JSON.stringify(data);
  for (const [client] of room.clients) {
    if (client !== sender && client.readyState === 1) client.send(msg);
  }
}

function getClientIp(req) {
  const f = req.headers["x-forwarded-for"];
  if (f) return f.split(",")[0].trim();
  return req.socket?.remoteAddress || "unknown";
}

function checkFileRate(ip) {
  const now = Date.now();
  let e = ipFileCounts.get(ip);
  if (!e || now > e.resetAt) { e = { count: 0, resetAt: now + 60_000 }; ipFileCounts.set(ip, e); }
  e.count++;
  return e.count <= MAX_FILES_PER_MIN;
}

function checkMsgRate(ip) {
  const now = Date.now();
  let e = ipMsgCounts.get(ip);
  if (!e || now > e.resetAt) { e = { count: 0, resetAt: now + 60_000 }; ipMsgCounts.set(ip, e); }
  e.count++;
  return e.count <= MAX_MSGS_PER_MIN;
}

function dropConnections(ip) {
  const conns = ipConnections.get(ip);
  if (conns) for (const c of conns) { try { c.close(4003, "banned"); } catch {} }
}

// Constant-time-ish comparison so admin access can't be timed. Returns false
// whenever no secret is configured, disabling the admin endpoints entirely.
function adminAuthorized(url) {
  if (!ADMIN_SECRET) return false;
  const provided = url.searchParams.get("key") || "";
  if (provided.length !== ADMIN_SECRET.length) return false;
  let diff = 0;
  for (let i = 0; i < provided.length; i++) diff |= provided.charCodeAt(i) ^ ADMIN_SECRET.charCodeAt(i);
  return diff === 0;
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

function getServiceAccount() {
  const b64 = process.env.GOOGLE_PLAY_SERVICE_ACCOUNT_JSON_B64;
  const raw = b64 ? Buffer.from(b64, "base64").toString("utf8") : process.env.GOOGLE_PLAY_SERVICE_ACCOUNT_JSON;
  if (!raw) throw new Error("Missing GOOGLE_PLAY_SERVICE_ACCOUNT_JSON_B64");
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

async function getGoogleAccessToken() {
  if (cachedGoogleToken && cachedGoogleToken.expiresAtMs > Date.now() + 60_000) return cachedGoogleToken.token;
  const assertion = await signJwt(getServiceAccount());
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
async function verifyPlayToken(purchaseToken) {
  if (!purchaseToken) return false;
  const cached = playVerifyCache.get(purchaseToken);
  if (cached && Date.now() - cached.checkedAtMs < PLAY_VERIFY_TTL_MS) return cached.activeUntilMs > Date.now();
  const accessToken = await getGoogleAccessToken();
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

const server = createServer((req, res) => {
  const ip = getClientIp({ headers: req.headers, socket: req.socket });

  if (req.url === "/health") {
    res.writeHead(200, { "Content-Type": "application/json" });
    res.end(JSON.stringify({ status: "ok", rooms: rooms.size, uptime: Math.floor((Date.now() - startTime) / 1000), stats }));
    return;
  }

  if (req.url?.startsWith("/admin/logs")) {
    const url = new URL(req.url, "http://localhost");
    if (!adminAuthorized(url)) { res.writeHead(404).end(); return; }
    const since = parseInt(url.searchParams.get("since") || "0");
    const ipFilter = url.searchParams.get("ip") || "";
    const level = url.searchParams.get("level") || "";
    let filtered = activityLog;
    if (ipFilter) filtered = filtered.filter(e => e.clientIp === ipFilter);
    if (level) filtered = filtered.filter(e => e.level === level);
    if (since) filtered = filtered.filter(e => new Date(e.timestamp).getTime() > since);
    res.writeHead(200, { "Content-Type": "application/json" });
    res.end(JSON.stringify({ total: filtered.length, entries: filtered.slice(-500) }));
    return;
  }

  if (req.url?.startsWith("/admin/stats")) {
    const url = new URL(req.url, "http://localhost");
    if (!adminAuthorized(url)) { res.writeHead(404).end(); return; }
    const activeIps = Array.from(ipConnections.entries()).map(([ip, s]) => ({ ip, connections: s.size })).sort((a, b) => b.connections - a.connections);
    res.writeHead(200, { "Content-Type": "application/json" });
    res.end(JSON.stringify({ rooms: rooms.size, activeIps, banned: [...bannedIps], stats, fileRate: Array.from(ipFileCounts.entries()).map(([ip, e]) => ({ ip, filesLastMin: e.count })) }));
    return;
  }

  // Ban an IP: /admin/unban?ip=1.2.3.4&key=SECRET (check unban before ban).
  if (req.url?.startsWith("/admin/unban")) {
    const url = new URL(req.url, "http://localhost");
    if (!adminAuthorized(url)) { res.writeHead(404).end(); return; }
    const target = url.searchParams.get("ip") || "";
    bannedIps.delete(target);
    log({ level: "info", action: "ip_unbanned", clientIp: target });
    res.writeHead(200, { "Content-Type": "application/json" });
    res.end(JSON.stringify({ banned: [...bannedIps] }));
    return;
  }

  if (req.url?.startsWith("/admin/ban")) {
    const url = new URL(req.url, "http://localhost");
    if (!adminAuthorized(url)) { res.writeHead(404).end(); return; }
    const target = url.searchParams.get("ip") || "";
    if (target) { bannedIps.add(target); dropConnections(target); log({ level: "warn", action: "ip_banned", clientIp: target }); }
    res.writeHead(200, { "Content-Type": "application/json" });
    res.end(JSON.stringify({ banned: [...bannedIps] }));
    return;
  }

  res.writeHead(404).end();
});

const startTime = Date.now();
const wss = new WebSocketServer({ server, maxPayload: MAX_WS_PAYLOAD });

wss.on("connection", (ws, req) => {
  const clientIp = getClientIp(req);
  let deviceName = "unknown", roomCode = null, isHost = false;

  // Reject banned IPs and IPs that already hold too many connections.
  if (bannedIps.has(clientIp)) { try { ws.close(4003, "banned"); } catch {} return; }
  const existing = ipConnections.get(clientIp);
  if (existing && existing.size >= MAX_CONN_PER_IP) { try { ws.close(4029, "too many connections"); } catch {} return; }

  if (!ipConnections.has(clientIp)) ipConnections.set(clientIp, new Set());
  ipConnections.get(clientIp).add(ws);

  ws.on("message", async (raw) => {
    try {
      const msg = JSON.parse(raw.toString());
      const { type } = msg;

      if (type === "create_room") {
        if (rooms.size >= MAX_ROOMS) { ws.send(JSON.stringify({ type: "error", message: "Server full" })); return; }
        // Android clients must hold an active subscription to host a Remote room.
        // Desktop/other clients are not gated.
        if (msg.client === "android") {
          let entitled = false;
          try { entitled = await verifyPlayToken(msg.purchaseToken); } catch (e) { log({ level: "error", action: "play_verify_error", clientIp, error: e.message }); }
          if (!entitled) { ws.send(JSON.stringify({ type: "error", message: "subscription_required" })); return; }
        }
        roomCode = generateCode();
        deviceName = msg.deviceName || "Host";
        isHost = true;
        stats.roomsCreated++;
        // authHash is set by a follow-up `set_auth`. Until then the room is
        // `pending` and cannot be joined.
        rooms.set(roomCode, { clients: new Map([[ws, { deviceName, isHost: true }]]), messages: [], authHash: null, pending: true });
        ws.send(JSON.stringify({ type: "room_created", roomCode }));
        log({ level: "info", action: "room_created", roomCode, deviceName, clientIp });
        return;
      }

      if (type === "set_auth") {
        const room = rooms.get(roomCode);
        if (!room || !isHost) return;
        if (typeof msg.authHash !== "string" || msg.authHash.length < 16) { ws.send(JSON.stringify({ type: "error", message: "Invalid room password" })); return; }
        room.authHash = msg.authHash;
        room.pending = false;
        return;
      }

      if (type === "join_room") {
        const code = msg.roomCode;
        const room = rooms.get(code);
        if (!room) { ws.send(JSON.stringify({ type: "error", message: "Room not found" })); return; }
        if (room.pending || !room.authHash) { ws.send(JSON.stringify({ type: "error", message: "Room not ready" })); return; }
        // The relay never sees the password, only this derived join hash.
        if (msg.authHash !== room.authHash) { ws.send(JSON.stringify({ type: "error", message: "Wrong password" })); return; }
        roomCode = code;
        deviceName = msg.deviceName || "Joiner";
        room.clients.set(ws, { deviceName, isHost: false });
        ws.send(JSON.stringify({ type: "room_joined", roomCode: code, messages: room.messages }));
        broadcast(room, ws, { type: "user_joined", deviceName });
        log({ level: "info", action: "user_joined", roomCode, deviceName, clientIp });
        return;
      }

      if (type === "message") {
        const room = rooms.get(roomCode);
        if (!room) return;
        if (typeof msg.payload !== "string") return;
        if (msg.payload.length > MAX_MESSAGE_SIZE) { ws.send(JSON.stringify({ type: "error", message: "Message too large" })); return; }
        if (!checkMsgRate(clientIp)) { ws.send(JSON.stringify({ type: "error", message: "Too many messages. Please slow down." })); return; }
        stats.totalMessages++;
        // payload is opaque ciphertext; the relay cannot read it.
        const entry = { sender: "user", deviceName, payload: msg.payload, createdAt: Date.now() };
        room.messages.push(entry);
        broadcast(room, ws, { type: "message", ...entry });
        return;
      }

      if (type === "file") {
        const room = rooms.get(roomCode);
        if (!room) { ws.send(JSON.stringify({ type: "error", message: "Room not found" })); return; }
        if (typeof msg.payload !== "string") return;
        if (msg.payload.length > MAX_FILE_SIZE) { ws.send(JSON.stringify({ type: "error", message: "File too large" })); return; }
        if (!checkMsgRate(clientIp)) { ws.send(JSON.stringify({ type: "error", message: "Too many messages. Please slow down." })); return; }
        if (!checkFileRate(clientIp)) {
          log({ level: "warn", action: "rate_limit_hit", clientIp, deviceName, roomCode });
          ws.send(JSON.stringify({ type: "error", message: "Too many files. Please wait." }));
          return;
        }
        const fileSize = msg.fileSize || 0;
        stats.totalFiles++;
        stats.totalBytesRelayed += msg.payload.length;
        // File name and contents are inside the encrypted payload; the relay only
        // sees the raw size for abuse control.
        log({ level: "info", action: "file_sent", roomCode, deviceName, clientIp, fileSize });
        const fm = { type: "file", deviceName, payload: msg.payload, fileSize, createdAt: Date.now() };
        room.messages.push({ sender: "user", deviceName, payload: msg.payload, fileSize, kind: "file", createdAt: Date.now() });
        broadcast(room, ws, fm);
        return;
      }

      if (type === "get_history") { const r = rooms.get(roomCode); if (r) ws.send(JSON.stringify({ type: "history", messages: r.messages })); return; }

      if (type === "leave") {
        const room = rooms.get(roomCode);
        if (!room) return;
        room.clients.delete(ws);
        if (isHost) { broadcast(room, null, { type: "room_closed" }); rooms.delete(roomCode); log({ level: "info", action: "room_closed", roomCode, deviceName, clientIp }); }
        else { broadcast(room, ws, { type: "user_left", deviceName }); }
        return;
      }
    } catch (err) {
      log({ level: "error", action: "parse_error", clientIp, error: err.message });
    }
  });

  ws.on("close", () => {
    const ic = ipConnections.get(clientIp);
    if (ic) { ic.delete(ws); if (ic.size === 0) ipConnections.delete(clientIp); }
    const room = rooms.get(roomCode);
    if (!room) return;
    room.clients.delete(ws);
    if (isHost) { broadcast(room, null, { type: "room_closed" }); rooms.delete(roomCode); log({ level: "info", action: "room_closed", roomCode, deviceName, clientIp, reason: "disconnect" }); }
    else { broadcast(room, null, { type: "user_left", deviceName }); }
  });
});

server.listen(PORT, () => console.log(`SideWire Relay running on port ${PORT}`));