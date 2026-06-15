import { Component, useCallback, useEffect, useRef, useState } from "react";
import type { ErrorInfo, ReactNode } from "react";
import {
  Bookmark,
  ChevronLeft,
  ChevronRight,
  Clipboard,
  Download,
  FileText,
  History,
  Link2,
  Lock,
  LogOut,
  Menu,
  Moon,
  Paperclip,
  Send,
  ShieldCheck,
  Sun,
  Terminal,
  Trash2,
  Users,
  X,
} from "lucide-react";
import { invoke } from "@tauri-apps/api/core";
import { save } from "@tauri-apps/plugin-dialog";
import { isPermissionGranted, requestPermission, sendNotification } from "@tauri-apps/plugin-notification";
import { platform } from "@tauri-apps/plugin-os";
import { getProducts, getProductStatus, purchase, acknowledgePurchase } from "@choochmeque/tauri-plugin-iap-api";
import "./App.css";

type LinkMessage = {
  id: string;
  sender: string;
  deviceName: string;
  kind: string;
  body: string;
  fileName?: string;
  fileSize?: number;
  fileMime?: string;
  downloadUrl?: string;
  payload?: string; // encrypted relay payload, used to save received files
  createdAt: number;
};

type RoomInfo = { roomCode: string; deviceName: string; devices: string[]; port: number; ip: string; hosting?: boolean; };
type DiscoveredRoom = { roomCode: string; ip: string; port: number };
type SavedConversation = { id: string; label: string; savedAt: number; messages: LinkMessage[] };
type ThemeMode = "dark" | "light";
type RoomMode = "local" | "remote";
// How the active room exchanges data:
//  host   - this app created a local room; others talk to our HTTP server
//  client - this app joined someone else's local room over the LAN
//  relay  - end-to-end encrypted room over the cloud relay
type Transport = "host" | "client" | "relay";

const APP_NAME = "SideWire";
const BRAND_PROMPT = "sidewire";
const THEME_STORAGE_KEY = "sidewire-theme";
const ROOM_JOINED_KEY = "sidewire-room-joined";
const DEVICE_NAME_KEY = "sidewire-device-name";
const ROOM_CODE_KEY = "sidewire-room-code";
const ROOM_HOST_KEY = "sidewire-room-host";
const ROOM_TOKEN_KEY = "sidewire-room-token";
const ROOM_MODE_KEY = "sidewire-room-mode";
const RELAY_URL = "wss://sidewire-relay.fly.dev";
// Android only: hosting a Remote room requires this Google Play subscription.
// Other platforms (Windows/desktop) keep Remote free.
const REMOTE_PRODUCT_ID = "sidewire_remote_monthly";
// User-facing file size limits (see MAX_FILE_SIZE in relay/server.js and
// DefaultBodyLimit in src-tauri/src/lib.rs).
const LOCAL_MAX_LABEL = "500 MB";
const REMOTE_MAX_LABEL = "100 MB";

function readTheme(): ThemeMode { return localStorage.getItem(THEME_STORAGE_KEY) === "light" ? "light" : "dark"; }
function readTransport(): Transport {
  const v = localStorage.getItem(ROOM_MODE_KEY);
  if (v === "relay" || v === "remote") return "relay";
  if (v === "client") return "client";
  return "host";
}
function formatBytes(bytes?: number) {
  if (!bytes) return "0 B";
  const u = ["B", "KB", "MB", "GB"]; let v = bytes!, i = 0;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return `${v.toFixed(v >= 10 || i === 0 ? 0 : 1)} ${u[i]}`;
}
function formatTime(value: number) { return new Date(value).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }); }
function genId() { return `msg-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`; }

class ErrorBoundary extends Component<{ children: ReactNode }, { hasError: boolean; error: string }> {
  constructor(p: { children: ReactNode }) { super(p); this.state = { hasError: false, error: "" }; }
  static getDerivedStateFromError(_: Error) { return { hasError: true }; }
  componentDidCatch(error: Error, info: ErrorInfo) { this.setState({ error: `${error.message}\n${info.componentStack || ""}` }); }
  render() {
    if (this.state.hasError) return (
      <main className="app-shell"><div style={{ padding: 40, color: "#f0f0f0", textAlign: "center" }}>
        <h2>Something went wrong</h2>
        <pre style={{ color: "#e84a8a", fontSize: 13, marginTop: 16 }}>{this.state.error}</pre>
        <button style={{ marginTop: 20, padding: "10px 20px", cursor: "pointer" }} onClick={() => { this.setState({ hasError: false, error: "" }); window.location.reload(); }}>Reload app</button>
      </div></main>);
    return this.props.children;
  }
}

let notifiedIds = new Set<string>();
async function notifyNewMessage(msg: LinkMessage) {
  if (msg.sender === "system") return;
  if (notifiedIds.has(msg.id)) return;
  notifiedIds.add(msg.id);
  const text = msg.kind === "file" ? `Sent: ${msg.fileName || "file"}` : msg.body;
  let granted = await isPermissionGranted();
  if (!granted) { const p = await requestPermission(); granted = p === "granted"; }
  if (granted) sendNotification({ title: "SideWire", body: text });
}

function App() {
  const [theme, setTheme] = useState<ThemeMode>(readTheme);
  useEffect(() => { document.documentElement.dataset.theme = theme; }, [theme]);
  function toggleTheme() { setTheme(c => { const n = c === "dark" ? "light" : "dark"; localStorage.setItem(THEME_STORAGE_KEY, n); return n; }); }
  return <ErrorBoundary><RoomApp theme={theme} onToggleTheme={toggleTheme} /></ErrorBoundary>;
}

function ThemeToggle({ theme, onToggleTheme }: { theme: ThemeMode; onToggleTheme: () => void }) {
  const Icon = theme === "dark" ? Sun : Moon;
  return <button type="button" className="theme-toggle" onClick={onToggleTheme} title="Switch theme"><Icon size={17} /><span>{theme === "dark" ? "Light" : "Dark"}</span></button>;
}

function MessageTranscript({ messages, onSaveFile, selfName }: { messages: LinkMessage[]; onSaveFile?: (m: LinkMessage) => void; selfName?: string }) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => { ref.current?.scrollTo({ top: ref.current.scrollHeight, behavior: "smooth" }); }, [messages.length]);
  return <div className="transcript" ref={ref}>
    {messages.map(m => {
      const cls = m.sender === "system" ? "system" : (m.deviceName && selfName && m.deviceName === selfName) ? "mine" : "other";
      return (
      <article className={`message ${cls}`} key={m.id}>
        <div className="message-meta"><span>{m.deviceName || m.sender}</span><time>{formatTime(m.createdAt)}</time></div>
        <p>{m.body}</p>
        {m.kind === "file" && <div className="file-card">
          <FileText size={20} />
          <div><strong>{m.fileName}</strong><span>{formatBytes(m.fileSize)}</span></div>
          {onSaveFile && (m.payload || m.downloadUrl) && <button type="button" className="file-save-btn" title="Save file" onClick={() => onSaveFile(m)}><Download size={16} /><span>Save</span></button>}
        </div>}
      </article>
      );
    })}
  </div>;
}

function RoomApp({ theme, onToggleTheme }: { theme: ThemeMode; onToggleTheme: () => void }) {
  const [roomInfo, setRoomInfo] = useState<RoomInfo | null>(null);
  const [messages, setMessages] = useState<LinkMessage[]>([]);
  const [draft, setDraft] = useState("");
  const [copied, setCopied] = useState(false);
  const [error, setError] = useState("");
  const [view, setView] = useState<"lobby" | "create" | "join" | "chat">(() => {
    const joined = localStorage.getItem(ROOM_JOINED_KEY) === "true";
    // Relay sessions are in-memory on the server and cannot be restored after a restart.
    return joined && readTransport() !== "relay" ? "chat" : "lobby";
  });
  const [myDeviceName, setMyDeviceName] = useState(() => localStorage.getItem(DEVICE_NAME_KEY) || "");
  const [roomMode, setRoomMode] = useState<RoomMode>("local"); // create-view selection only
  const [transport, setTransport] = useState<Transport>(readTransport);
  const [roomPassword, setRoomPassword] = useState("");
  const [joinCode, setJoinCode] = useState("");
  const [joinPassword, setJoinPassword] = useState("");
  const [joinDeviceName, setJoinDeviceName] = useState(() => localStorage.getItem(DEVICE_NAME_KEY) || "");
  const [scanning, setScanning] = useState(false);
  const [busy, setBusy] = useState<"" | "creating" | "opening" | "joining">("");
  const [foundRooms, setFoundRooms] = useState<DiscoveredRoom[]>([]);
  const messagesRef = useRef<LinkMessage[]>([]);
  const [savedConversations, setSavedConversations] = useState<SavedConversation[]>([]);
  const [viewingSaved, setViewingSaved] = useState<SavedConversation | null>(null);
  const wsRef = useRef<WebSocket | null>(null);
  const [isHost, setIsHost] = useState(true);

  const transportRef = useRef<Transport>(transport);
  useEffect(() => { transportRef.current = transport; }, [transport]);
  const hostRef = useRef<string>(localStorage.getItem(ROOM_HOST_KEY) || "");
  const tokenRef = useRef<string>(localStorage.getItem(ROOM_TOKEN_KEY) || "");
  const remoteRoomReadyRef = useRef(false);
  const relayCodeRef = useRef<string>(""); // remembers the relay room code across mode toggles

  // Remote subscription gate. Gated stores: Android (Play, relay-verified) and
  // Windows (Microsoft Store, client-side). macOS/Linux: free.
  const platformRef = useRef<string>("");
  const isAndroidRef = useRef(false);
  const gatedPlatformRef = useRef(false); // android or windows -> Remote needs a sub
  const purchaseTokenRef = useRef<string>("");
  const [remoteEntitled, setRemoteEntitled] = useState(false);
  const [showPaywall, setShowPaywall] = useState(false);
  const [paywallPrice, setPaywallPrice] = useState("");
  const [paywallBusy, setPaywallBusy] = useState(false);

  const refreshEntitlement = useCallback(async () => {
    try {
      const status: any = await getProductStatus(REMOTE_PRODUCT_ID, "subs");
      const active = !!status?.isOwned;
      if (status?.purchaseToken) purchaseTokenRef.current = status.purchaseToken;
      setRemoteEntitled(active);
      return active;
    } catch { return false; }
  }, []);

  useEffect(() => {
    (async () => {
      let p = ""; try { p = await platform(); } catch {}
      platformRef.current = p;
      isAndroidRef.current = p === "android";
      gatedPlatformRef.current = p === "android" || p === "windows";
      if (gatedPlatformRef.current) await refreshEntitlement();
      else setRemoteEntitled(true); // macOS/Linux: Remote is free
    })();
  }, [refreshEntitlement]);

  async function openPaywall() {
    setShowPaywall(true); setError(""); setPaywallPrice("");
    try {
      const r: any = await getProducts([REMOTE_PRODUCT_ID], "subs");
      const products: any[] = Array.isArray(r) ? r : (r?.products ?? []);
      const offer = products?.[0]?.subscriptionOfferDetails?.[0];
      const phase = offer?.pricingPhases?.pricingPhaseList?.[0] ?? offer?.pricingPhases?.[0];
      setPaywallPrice(phase?.formattedPrice || "$2.99/month");
    } catch { setPaywallPrice("$2.99/month"); }
  }

  async function subscribeRemote() {
    if (paywallBusy) return;
    setPaywallBusy(true); setError("");
    try {
      const r: any = await getProducts([REMOTE_PRODUCT_ID], "subs");
      const products: any[] = Array.isArray(r) ? r : (r?.products ?? []);
      const offerToken = products?.[0]?.subscriptionOfferDetails?.[0]?.offerToken;
      const result: any = await purchase(REMOTE_PRODUCT_ID, "subs", offerToken ? { offerToken } : undefined);
      const token = result?.purchaseToken;
      if (token) { purchaseTokenRef.current = token; try { await acknowledgePurchase(token); } catch {} }
      const active = await refreshEntitlement();
      if (active || token) { setRemoteEntitled(true); setShowPaywall(false); }
      else setError("Purchase was not completed.");
    } catch (err) { setError(String(err)); }
    setPaywallBusy(false);
  }

  async function restorePurchase() {
    setPaywallBusy(true); setError("");
    const active = await refreshEntitlement();
    if (active) setShowPaywall(false); else setError("No active subscription found for this Google account.");
    setPaywallBusy(false);
  }

  // Mobile slide-in drawers (swipe right = links, swipe left = room info)
  const [leftOpen, setLeftOpen] = useState(false);
  const [rightOpen, setRightOpen] = useState(false);
  const touchStart = useRef<{ x: number; y: number } | null>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  function onChatTouchStart(e: React.TouchEvent) { const t = e.touches[0]; touchStart.current = { x: t.clientX, y: t.clientY }; }
  function onChatTouchEnd(e: React.TouchEvent) {
    const s = touchStart.current; touchStart.current = null; if (!s) return;
    const t = e.changedTouches[0]; const dx = t.clientX - s.x; const dy = t.clientY - s.y;
    if (Math.abs(dx) < 60 || Math.abs(dx) < Math.abs(dy) * 1.4) return; // ignore taps / vertical scrolls
    if (dx > 0) { if (rightOpen) setRightOpen(false); else setLeftOpen(true); }
    else { if (leftOpen) setLeftOpen(false); else setRightOpen(true); }
  }
  function closeDrawers() { setLeftOpen(false); setRightOpen(false); }

  function setTransportPersist(t: Transport) { transportRef.current = t; setTransport(t); localStorage.setItem(ROOM_MODE_KEY, t); }
  function systemMsg(body: string) { return { sender: "system", deviceName: "system", kind: "text", body, createdAt: Date.now() }; }
  function addLocalMessage(m: Partial<LinkMessage> & { body: string }) {
    const entry: LinkMessage = {
      id: genId(), sender: m.sender === "system" ? "system" : "user", deviceName: m.deviceName || "unknown",
      kind: m.kind || "text", body: m.body, fileName: m.fileName, fileSize: m.fileSize, fileMime: m.fileMime,
      payload: m.payload, downloadUrl: m.downloadUrl, createdAt: m.createdAt || Date.now(),
    };
    setMessages(prev => { const next = [...prev, entry]; messagesRef.current = next; return next; });
    if (entry.sender !== "system" && document.visibilityState === "hidden") notifyNewMessage(entry);
  }

  const refreshRoomInfo = useCallback(async () => { try { setRoomInfo(await invoke<RoomInfo>("get_room_info")); } catch {} }, []);
  const refreshMessages = useCallback(async () => {
    try {
      let next: LinkMessage[];
      if (transportRef.current === "client") {
        const host = hostRef.current, token = tokenRef.current;
        if (!host || !token) return;
        const r = await fetch(`http://${host}/api/messages?token=${encodeURIComponent(token)}`, { cache: "no-store" });
        if (!r.ok) return;
        next = await r.json();
      } else if (transportRef.current === "host") {
        next = await invoke<LinkMessage[]>("list_messages");
      } else { return; } // relay is push-based
      if (next.length > messagesRef.current.length && document.visibilityState === "hidden") { for (let i = messagesRef.current.length; i < next.length; i++) notifyNewMessage(next[i]); }
      messagesRef.current = next; setMessages(next); setError("");
    } catch {}
  }, []);
  const refreshSavedConversations = useCallback(async () => { try { setSavedConversations(await invoke<SavedConversation[]>("list_saved_conversations")); } catch {} }, []);

  useEffect(() => {
    if (view !== "chat") return;
    refreshSavedConversations();
    if (transport === "relay") return; // relay pushes over the websocket
    if (transport === "host") refreshRoomInfo();
    refreshMessages();
    const id = window.setInterval(() => { if (transportRef.current === "host") refreshRoomInfo(); refreshMessages(); }, 1500);
    return () => window.clearInterval(id);
  }, [view, transport, refreshRoomInfo, refreshMessages, refreshSavedConversations]);

  // ── Relay (remote, end-to-end encrypted) ──

  async function mapRelayHistory(items: any[]): Promise<LinkMessage[]> {
    const out: LinkMessage[] = [];
    for (const m of items) {
      if (m.kind === "file") {
        let meta = { fileName: "file", fileMime: "", fileSize: m.fileSize || 0 };
        try { meta = await invoke("remote_file_meta", { payload: m.payload }); } catch {}
        out.push({ id: genId(), sender: "user", deviceName: m.deviceName, kind: "file", body: `sent ${meta.fileName}`, fileName: meta.fileName, fileSize: meta.fileSize, fileMime: meta.fileMime, payload: m.payload, createdAt: m.createdAt });
      } else {
        let body = "[unable to decrypt]";
        try { body = await invoke<string>("remote_decrypt_text", { payload: m.payload }); } catch {}
        out.push({ id: genId(), sender: m.sender === "system" ? "system" : "user", deviceName: m.deviceName, kind: "text", body, createdAt: m.createdAt });
      }
    }
    return out;
  }

  async function handleRelayEvent(msg: any) {
    switch (msg.type) {
      case "room_created":
        relayCodeRef.current = msg.roomCode;
        localStorage.setItem(ROOM_CODE_KEY, msg.roomCode);
        setRoomInfo({ roomCode: msg.roomCode, deviceName: myDeviceName, devices: [], port: 0, ip: "" });
        setError("");
        break;
      case "room_joined": {
        localStorage.setItem(ROOM_CODE_KEY, msg.roomCode);
        setRoomInfo({ roomCode: msg.roomCode, deviceName: myDeviceName, devices: [], port: 0, ip: "" });
        const mapped = msg.messages ? await mapRelayHistory(msg.messages) : [];
        messagesRef.current = mapped; setMessages(mapped);
        setTransportPersist("relay"); setIsHost(false);
        localStorage.setItem(ROOM_JOINED_KEY, "true");
        setBusy(""); setError(""); setView("chat");
        break;
      }
      case "message": {
        let body = "[unable to decrypt]";
        try { body = await invoke<string>("remote_decrypt_text", { payload: msg.payload }); } catch {}
        addLocalMessage({ sender: msg.sender, deviceName: msg.deviceName, kind: "text", body, createdAt: msg.createdAt });
        break;
      }
      case "file": {
        let meta = { fileName: "file", fileMime: "", fileSize: msg.fileSize || 0 };
        try { meta = await invoke("remote_file_meta", { payload: msg.payload }); } catch {}
        addLocalMessage({ sender: "user", deviceName: msg.deviceName, kind: "file", body: `sent ${meta.fileName}`, fileName: meta.fileName, fileSize: meta.fileSize, fileMime: meta.fileMime, payload: msg.payload, createdAt: msg.createdAt });
        break;
      }
      case "user_joined": addLocalMessage(systemMsg(`${msg.deviceName} joined`)); break;
      case "user_left": addLocalMessage(systemMsg(`${msg.deviceName} left`)); break;
      case "room_closed": addLocalMessage(systemMsg("Room closed by host")); break;
      case "error":
        setBusy("");
        // Relay rejected an unsubscribed Android host: show the paywall.
        if (msg.message === "subscription_required") {
          remoteRoomReadyRef.current = false; relayCodeRef.current = "";
          if (wsRef.current) { wsRef.current.close(); wsRef.current = null; }
          setRemoteEntitled(false); setView("create"); openPaywall();
          break;
        }
        setError(msg.message || "Relay error");
        // Join failures: bounce back to the join screen.
        if (/password|not found|not ready|full/i.test(msg.message || "") && !isHost) {
          if (wsRef.current) { wsRef.current.close(); wsRef.current = null; }
          localStorage.removeItem(ROOM_JOINED_KEY); setView("join");
        }
        break;
    }
  }

  function connectRelay(url: string, onReady?: () => void) {
    if (wsRef.current) wsRef.current.close();
    const ws = new WebSocket(url);
    wsRef.current = ws;
    // Send the create/join only once the socket is actually open. The relay
    // machine can cold-start (Fly.io), so a fixed timer would race and hang.
    ws.onopen = () => { onReady?.(); };
    ws.onmessage = (event) => { try { handleRelayEvent(JSON.parse(event.data)); } catch {} };
    ws.onerror = () => { setError("Could not connect to relay server."); remoteRoomReadyRef.current = false; setBusy(""); };
    ws.onclose = () => {};
  }

  function sendRelay(type: string, data: any = {}) { if (wsRef.current?.readyState === WebSocket.OPEN) wsRef.current.send(JSON.stringify({ type, deviceName: myDeviceName || "Me", ...data })); }

  // ── Create flow ──

  async function createRoom() {
    if (busy) return;
    setBusy("creating");
    const name = myDeviceName.trim() || "My Device";
    localStorage.setItem(DEVICE_NAME_KEY, name);
    remoteRoomReadyRef.current = false;
    relayCodeRef.current = "";
    if (wsRef.current) { wsRef.current.close(); wsRef.current = null; }
    setRoomMode("local");
    try { setRoomInfo(await invoke<RoomInfo>("get_room_info")); setView("create"); } catch (err) { setError(String(err)); }
    finally { setBusy(""); }
  }

  function chooseCreateMode(mode: RoomMode) {
    setError("");
    if (mode === "remote") {
      // Android/Windows: hosting Remote requires an active subscription.
      if (gatedPlatformRef.current && !remoteEntitled) { setRoomMode("remote"); openPaywall(); return; }
      setRoomMode("remote");
      if (relayCodeRef.current) {
        setRoomInfo({ roomCode: relayCodeRef.current, deviceName: myDeviceName, devices: [], port: 0, ip: "" });
      } else {
        setRoomInfo({ roomCode: "Connecting...", deviceName: myDeviceName, devices: [], port: 0, ip: "" });
        if (!remoteRoomReadyRef.current) {
          remoteRoomReadyRef.current = true;
          // Relay verifies Android tokens; Windows is gated client-side, so it
          // sends no token and the relay treats it like any non-android client.
          connectRelay(RELAY_URL, () => sendRelay("create_room", { client: platformRef.current || "desktop", purchaseToken: isAndroidRef.current ? purchaseTokenRef.current : undefined }));
        }
      }
    } else {
      setRoomMode("local");
      refreshRoomInfo();
    }
  }

  async function openRoom() {
    if (busy) return;
    setBusy("opening");
    try {
      localStorage.setItem(DEVICE_NAME_KEY, myDeviceName.trim() || "My Device");
      if (roomMode === "remote") {
        const code = roomInfo?.roomCode;
        if (!code || code === "Connecting...") { setError("Still connecting to the relay, try again in a moment."); return; }
        if (!roomPassword.trim()) { setError("Set a room password - it is required for remote rooms."); return; }
        const res = await invoke<{ authHash: string }>("remote_set_key", { password: roomPassword.trim(), roomCode: code });
        sendRelay("set_auth", { authHash: res.authHash });
        setTransportPersist("relay"); setIsHost(true);
        localStorage.setItem(ROOM_JOINED_KEY, "true");
        setView("chat");
        addLocalMessage(systemMsg(`Room opened. Share code: ${code}`));
      } else {
        if (wsRef.current) { wsRef.current.close(); wsRef.current = null; }
        setTransportPersist("host"); setIsHost(true);
        invoke("start_local_hosting").catch(() => {}); // begin broadcasting now, not at launch
        localStorage.setItem(ROOM_JOINED_KEY, "true");
        setView("chat"); refreshRoomInfo(); refreshMessages();
      }
    } catch (err) { setError(String(err)); }
    finally { setBusy(""); }
  }

  function cancelCreate() {
    if (wsRef.current) { wsRef.current.close(); wsRef.current = null; }
    remoteRoomReadyRef.current = false; relayCodeRef.current = "";
    setView("lobby"); setError("");
  }

  async function copyRoomCode() { if (!roomInfo) return; try { await navigator.clipboard.writeText(roomInfo.roomCode); setCopied(true); window.setTimeout(() => setCopied(false), 1400); } catch {} }

  // Actively probe the local /24 subnet over HTTP. This works even when Android
  // Wi-Fi drops incoming UDP broadcasts, so the user never has to type an IP.
  async function sweepLan(): Promise<DiscoveredRoom[]> {
    let me: RoomInfo;
    try { me = await invoke<RoomInfo>("get_room_info"); } catch { return []; }
    const ip = me.ip || "";
    if (!ip || ip.startsWith("127.")) return [];
    const base = ip.replace(/\.\d+$/, ".");
    const ownCode = me.roomCode;
    const found: DiscoveredRoom[] = [];
    const port = 8765; // primary LAN port the host binds (see LAN_PORT_START in Rust)
    const probe = async (host: string) => {
      try {
        const ctrl = new AbortController();
        const timer = setTimeout(() => ctrl.abort(), 800);
        const r = await fetch(`http://${host}:${port}/api/room`, { cache: "no-store", signal: ctrl.signal });
        clearTimeout(timer);
        if (r.ok) { const info: RoomInfo = await r.json(); if (info.roomCode && info.roomCode !== ownCode && info.hosting) found.push({ roomCode: info.roomCode, ip: host, port }); }
      } catch {}
    };
    const hosts: string[] = [];
    for (let i = 1; i <= 254; i++) hosts.push(base + i);
    const BATCH = 51;
    for (let i = 0; i < hosts.length; i += BATCH) await Promise.all(hosts.slice(i, i + BATCH).map(probe));
    return found;
  }

  // Combine the HTTP subnet sweep with UDP discovery (bonus when broadcast works).
  async function discoverLan(): Promise<DiscoveredRoom[]> {
    const [sweep, udp] = await Promise.all([
      sweepLan(),
      invoke<DiscoveredRoom[]>("discover_rooms", { timeoutSecs: 2 }).catch(() => [] as DiscoveredRoom[]),
    ]);
    const byCode = new Map<string, DiscoveredRoom>();
    for (const r of [...sweep, ...udp]) if (r.roomCode && !byCode.has(r.roomCode)) byCode.set(r.roomCode, r);
    return [...byCode.values()];
  }

  async function scanNetwork() {
    setScanning(true); setFoundRooms([]); setError("Scanning your Wi-Fi network...");
    try {
      const rooms = await discoverLan();
      setFoundRooms(rooms);
      setError(rooms.length === 0 ? "No rooms found. Make sure the other device opened a Local room on the same Wi-Fi." : "");
    } catch (err) { setError(String(err)); }
    setScanning(false);
  }

  // ── Join flow ──

  async function resolvePort(ip: string, code: string, known: number): Promise<number> {
    if (known > 0) return known;
    // Scan the known LAN port range the host binds to (see LAN_PORT_START in Rust).
    for (let p = 8765; p <= 8784; p++) {
      try { const r = await fetch(`http://${ip}:${p}/api/room`, { cache: "no-store" }); if (r.ok) { const i: RoomInfo = await r.json(); if (i.roomCode === code) return p; } } catch {}
    }
    return 0;
  }

  async function joinLocalHost(ip: string, port: number, code: string, name: string): Promise<boolean> {
    try {
      const r = await fetch(`http://${ip}:${port}/api/join?device=${encodeURIComponent(name)}`, { cache: "no-store" });
      if (!r.ok) return false;
      const d = await r.json();
      hostRef.current = `${ip}:${port}`; tokenRef.current = d.token;
      localStorage.setItem(ROOM_HOST_KEY, hostRef.current); localStorage.setItem(ROOM_TOKEN_KEY, d.token);
      localStorage.setItem(ROOM_CODE_KEY, code); localStorage.setItem(ROOM_JOINED_KEY, "true");
      setTransportPersist("client"); setIsHost(false);
      setRoomInfo({ roomCode: code, deviceName: "host", devices: [], port, ip });
      setView("chat"); setError("");
      return true;
    } catch { return false; }
  }

  async function joinRoom(room?: DiscoveredRoom) {
    if (busy) return;
    const code = (room?.roomCode || joinCode.trim().toUpperCase());
    const name = joinDeviceName.trim() || myDeviceName.trim() || "My Device";
    const pass = joinPassword.trim();
    if (!code) { setError("Enter a room code."); return; }
    localStorage.setItem(DEVICE_NAME_KEY, name); setMyDeviceName(name);
    setBusy("joining");

    // A password means a remote room - go straight to the relay, no Wi-Fi scan.
    if (pass && !room) {
      setError("Connecting to the room...");
      try {
        const res = await invoke<{ authHash: string }>("remote_set_key", { password: pass, roomCode: code });
        connectRelay(RELAY_URL, () => sendRelay("join_room", { roomCode: code, authHash: res.authHash }));
        // busy stays set until handleRelayEvent receives room_joined or error
      } catch (err) { setError(String(err)); setBusy(""); }
      return;
    }

    // No password - it is a local room. Find it on the Wi-Fi.
    setError(room ? "Connecting..." : "Looking for the room on your Wi-Fi...");
    try {
      // Reuse rooms a prior Scan already found; only re-sweep if needed.
      let target: DiscoveredRoom | null = room || foundRooms.find(r => r.roomCode.toUpperCase() === code) || null;
      if (!target) { try { const rooms = await discoverLan(); target = rooms.find(r => r.roomCode.toUpperCase() === code) || null; } catch {} }
      if (target) {
        const port = await resolvePort(target.ip, code, target.port);
        if (port > 0 && await joinLocalHost(target.ip, port, code, name)) return;
      }
      setError("Could not find that room on your Wi-Fi. If it is a remote room, enter its password to join.");
    } finally { setBusy(""); }
  }

  // ── Messaging / files ──

  async function sendMessage(event: React.FormEvent) {
    event.preventDefault();
    const text = draft.trim();
    if (!text) return;
    setDraft("");
    if (transport === "relay") {
      try { const payload = await invoke<string>("remote_encrypt_text", { text }); sendRelay("message", { payload }); } catch { return; }
      addLocalMessage({ sender: "user", deviceName: myDeviceName || "Me", kind: "text", body: text });
    } else if (transport === "client") {
      try { await fetch(`http://${hostRef.current}/api/message?token=${encodeURIComponent(tokenRef.current)}&device=${encodeURIComponent(myDeviceName || "Me")}`, { method: "POST", body: text }); } catch {}
      await refreshMessages();
    } else {
      try { await invoke<LinkMessage>("send_text", { text, deviceName: myDeviceName || "Me" }); } catch {}
      await refreshMessages();
    }
  }

  // Read a File's bytes as base64 using the browser, which works on every
  // platform - including Android, where the picker returns a content:// URI
  // that the Rust filesystem APIs cannot read.
  function fileToBase64(file: File): Promise<string> {
    return new Promise((resolve, reject) => {
      const reader = new FileReader();
      reader.onload = () => resolve((reader.result as string).split(",")[1] || "");
      reader.onerror = () => reject(reader.error);
      reader.readAsDataURL(file);
    });
  }

  function chooseFiles() { fileInputRef.current?.click(); }

  async function onFilesPicked(event: React.ChangeEvent<HTMLInputElement>) {
    const files = Array.from(event.target.files || []);
    event.target.value = ""; // allow picking the same file again
    for (const file of files) {
      const name = file.name || "file";
      const mime = file.type || "application/octet-stream";
      try {
        if (transport === "relay") {
          const b64 = await fileToBase64(file);
          const res = await invoke<{ payload: string; fileSize: number }>("remote_encrypt_bytes", { fileName: name, mime, dataBase64: b64 });
          sendRelay("file", { payload: res.payload, fileSize: res.fileSize });
          addLocalMessage({ sender: "user", deviceName: myDeviceName || "Me", kind: "file", body: `sent ${name}`, fileName: name, fileSize: res.fileSize, payload: res.payload });
        } else if (transport === "client") {
          const buf = await file.arrayBuffer(); // ArrayBuffer body keeps the request CORS-simple (no preflight)
          await fetch(`http://${hostRef.current}/api/upload?token=${encodeURIComponent(tokenRef.current)}&device=${encodeURIComponent(myDeviceName || "Me")}&name=${encodeURIComponent(name)}&mime=${encodeURIComponent(mime)}`, { method: "POST", body: buf });
        } else {
          const b64 = await fileToBase64(file);
          await invoke<LinkMessage>("share_file_bytes", { fileName: name, mime, dataBase64: b64, deviceName: myDeviceName || "Me" });
        }
      } catch (err) { setError(String(err)); }
    }
    if (transport !== "relay") await refreshMessages();
  }

  async function saveFile(m: LinkMessage) {
    try {
      if (transport === "relay") {
        if (!m.payload) return;
        const meta = await invoke<{ fileName: string }>("remote_file_meta", { payload: m.payload });
        const savePath = await save({ defaultPath: meta.fileName || m.fileName });
        if (!savePath) return;
        await invoke("remote_decrypt_file", { payload: m.payload, savePath });
      } else if (transport === "client") {
        if (!m.downloadUrl) return;
        const r = await fetch(`http://${hostRef.current}${m.downloadUrl}?token=${encodeURIComponent(tokenRef.current)}`, { cache: "no-store" });
        if (!r.ok) { setError("Could not download file."); return; }
        const keyHex = r.headers.get("X-Encryption-Key-Hex") || "";
        const origName = r.headers.get("X-Original-Name") || m.fileName || "file";
        const buf = new Uint8Array(await r.arrayBuffer());
        let bin = ""; for (let i = 0; i < buf.length; i++) bin += String.fromCharCode(buf[i]);
        const cipherBase64 = btoa(bin);
        const savePath = await save({ defaultPath: origName });
        if (!savePath) return;
        await invoke("save_local_download", { cipherBase64, keyHex, savePath });
      }
    } catch (err) { setError(String(err)); }
  }

  async function handleSaveConversation() { try { await invoke<SavedConversation>("save_conversation", { label: "" }); await refreshSavedConversations(); } catch {} }
  async function handleLoadConversation(conv: SavedConversation) { try { const loaded = await invoke<SavedConversation>("load_conversation", { id: conv.id }); setViewingSaved(loaded); closeDrawers(); } catch {} }
  async function handleDeleteConversation(id: string) { try { await invoke<void>("delete_conversation", { id }); if (viewingSaved?.id === id) setViewingSaved(null); await refreshSavedConversations(); } catch {} }
  async function handleClearMessages() {
    if (transport === "host") { try { await invoke<LinkMessage[]>("clear_messages"); await refreshMessages(); } catch {} }
    else { setMessages([]); messagesRef.current = []; }
  }
  function handleBackToLive() { setViewingSaved(null); }

  function leaveRoom() {
    if (transport === "relay" && wsRef.current) { sendRelay("leave"); wsRef.current.close(); wsRef.current = null; }
    remoteRoomReadyRef.current = false; relayCodeRef.current = "";
    invoke("remote_clear_key").catch(() => {});
    invoke("stop_local_hosting").catch(() => {}); // stop broadcasting when leaving
    hostRef.current = ""; tokenRef.current = "";
    localStorage.removeItem(ROOM_JOINED_KEY); localStorage.removeItem(ROOM_HOST_KEY); localStorage.removeItem(ROOM_TOKEN_KEY);
    setTransportPersist("host");
    closeDrawers();
    setView("lobby"); setMessages([]); messagesRef.current = []; setError("");
  }

  function getDevicesCount() {
    if (transport === "relay") return isHost ? "Hosting via relay" : "Joined via relay";
    if (transport === "client") return "Connected to host";
    return `${roomInfo?.devices.length || 1} device(s)`;
  }

  // PAYWALL (Android Remote subscription)
  if (showPaywall) return (
    <main className={`lobby-center theme-${theme}`}>
      <div className="lobby-card">
        <div className="brand-title" style={{ justifyContent: "center" }}><Lock size={26} /><span>Remote rooms</span></div>
        <p className="lobby-desc">Local rooms on the same Wi-Fi are always free. Hosting a room over the internet uses our relay - unlock Remote hosting for {paywallPrice || "$2.99/month"}.</p>
        <button className="lobby-btn lobby-btn-primary" onClick={subscribeRemote} disabled={paywallBusy}><Lock size={18} /><span>{paywallBusy ? "Processing..." : `Subscribe - ${paywallPrice || "$2.99/month"}`}</span></button>
        <button className="lobby-btn" onClick={restorePurchase} disabled={paywallBusy}><span>Restore purchase</span></button>
        <button className="lobby-btn" onClick={() => { setShowPaywall(false); setError(""); }} disabled={paywallBusy}><X size={18} /><span>Not now</span></button>
        {error && <div className="error-line" style={{ marginTop: 12 }}>{error}</div>}
      </div>
    </main>
  );

  // LOBBY
  if (view === "lobby") return (
    <main className={`lobby-center theme-${theme}`}>
      <div className="lobby-card">
        <div className="brand-title" style={{ justifyContent: "center" }}><Terminal size={28} /><span>{APP_NAME}</span></div>
        <p className="lobby-desc">Send notes and files between your devices - locally over Wi-Fi, or anywhere with end-to-end encryption. No accounts.</p>
        <div className="lobby-buttons">
          <button className="lobby-btn lobby-btn-primary" onClick={createRoom} disabled={busy === "creating"}><Users size={20} /><span>{busy === "creating" ? "Creating..." : "Create Room"}</span></button>
          <button className="lobby-btn" onClick={() => setView("join")} disabled={!!busy}><Link2 size={20} /><span>Join Room</span></button>
        </div>
        <ThemeToggle theme={theme} onToggleTheme={onToggleTheme} />
        {error && <div className="error-line" style={{ marginTop: 12 }}>{error}</div>}
      </div>
    </main>
  );

  // CREATE ROOM
  if (view === "create") return (
    <main className={`lobby-center theme-${theme}`}>
      <div className="lobby-card">
        <h2 className="create-title"><Terminal size={22} />Your Room</h2>
        <div className="room-code-display">
          <span className="room-code-text">{roomInfo?.roomCode || "Loading..."}</span>
          <button className="copy-code-btn" onClick={copyRoomCode} title="Copy room code"><Clipboard size={18} /><span>{copied ? "Copied" : "Copy"}</span></button>
        </div>
        <p className="create-hint">Share this code with others so they can join.</p>
        <div className="join-form-fields">
          <label>Your Name (how others see you)</label>
          <input value={myDeviceName} onChange={e => setMyDeviceName(e.target.value)} placeholder="e.g. My PC" className="lobby-name-input" />
        </div>
        <div className="room-mode-toggle">
          <button className={`lobby-btn mode-btn ${roomMode === "local" ? "lobby-btn-primary" : ""}`} onClick={() => chooseCreateMode("local")}><span className="mode-btn-main">Local</span><span className="mode-btn-sub">(same Wi-Fi)</span></button>
          <button className={`lobby-btn mode-btn ${roomMode === "remote" ? "lobby-btn-primary" : ""}`} onClick={() => chooseCreateMode("remote")}><span className="mode-btn-main">Remote</span><span className="mode-btn-sub">(anywhere)</span></button>
        </div>
        <div className="mode-caption-row">
          <span>Files up to {LOCAL_MAX_LABEL}</span>
          <span>Files up to {REMOTE_MAX_LABEL}</span>
        </div>
        {roomMode === "local" ? (
          <>
            <div className="create-ip-row"><span className="create-ip-label">Your IP:</span><span className="create-ip-value">{roomInfo?.ip || "..."}</span></div>
            <p className="create-hint" style={{ fontSize: 12, opacity: 0.6 }}>Other devices on the same Wi-Fi can find this room and connect directly. Stays on your local network.</p>
          </>
        ) : (
          <>
            <div className="room-password-row">
              <input value={roomPassword} onChange={e => setRoomPassword(e.target.value)} placeholder="Room password (required)" className="lobby-name-input" type="password" />
            </div>
            <p className="create-hint" style={{ fontSize: 12, opacity: 0.6 }}>Works anywhere through our relay. Messages and files are end-to-end encrypted with this password - the relay only forwards ciphertext and cannot read them. Share the password securely; without it the room cannot be joined or decrypted.</p>
            <p className="create-hint" style={{ fontSize: 11, opacity: 0.45, marginTop: -6 }}>The relay processes your IP address to route the connection. See <a href="https://eeriegoesd.com/privacy/sidewire/" target="_blank" rel="noreferrer" style={{ color: "#6db3ff" }}>Privacy policy</a>.</p>
          </>
        )}
        <button className="lobby-btn lobby-btn-primary" onClick={openRoom} disabled={busy === "opening"} style={{ marginTop: 8 }}><Send size={18} /><span>{busy === "opening" ? "Opening..." : "Open Room"}</span></button>
        <button className="lobby-btn" onClick={cancelCreate} disabled={busy === "opening"} style={{ marginTop: 4 }}><X size={18} /><span>Cancel</span></button>
        {error && <div className="error-line" style={{ marginTop: 12 }}>{error}</div>}
      </div>
    </main>
  );

  // JOIN ROOM
  if (view === "join") return (
    <main className={`lobby-center theme-${theme}`}>
      <div className="lobby-card">
        <h2 className="create-title"><Link2 size={22} />Join a Room</h2>
        <button className="lobby-btn lobby-btn-primary" onClick={scanNetwork} disabled={scanning || !!busy}><span>{scanning ? "Scanning..." : "Scan for rooms on this network"}</span></button>
        {foundRooms.length > 0 && <div className="found-rooms">
          <p style={{ color: "rgba(255,255,255,0.6)", fontSize: 13, margin: "8px 0 4px" }}>Found rooms:</p>
          {foundRooms.map(r => (
            <button key={r.roomCode} className="lobby-btn" onClick={() => joinRoom(r)} disabled={!!busy} style={{ width: "100%", marginBottom: 4, justifyContent: "space-between" }}>
              <span><Users size={16} /> {r.roomCode}</span>
              <span style={{ fontSize: 12, opacity: 0.6 }}>{busy === "joining" ? "Joining..." : r.ip}</span>
            </button>
          ))}
        </div>}
        <div className="join-separator"><span>or enter code manually</span></div>
        <div className="join-form-fields">
          <label>Room Code</label>
          <input value={joinCode} onChange={e => setJoinCode(e.target.value.toUpperCase())} placeholder="e.g. 7K2Q9X" className="lobby-name-input" />
          <label style={{ marginTop: 10 }}>Password (only for remote rooms)</label>
          <input value={joinPassword} onChange={e => setJoinPassword(e.target.value)} placeholder="leave blank for local rooms" className="lobby-name-input" type="password" />
          <label style={{ marginTop: 10 }}>Your Name (how others see you)</label>
          <input value={joinDeviceName} onChange={e => setJoinDeviceName(e.target.value)} placeholder="Tap to type your name, e.g. My Phone" className="lobby-name-input" />
        </div>
        <button className="lobby-btn lobby-btn-primary" onClick={() => joinRoom()} disabled={busy === "joining"}><Link2 size={18} /><span>{busy === "joining" ? "Joining..." : "Join Room"}</span></button>
        <button className="lobby-btn" onClick={() => setView("lobby")}><X size={18} /><span>Back</span></button>
        {error && <div className="error-line" style={{ marginTop: 12 }}>{error}</div>}
      </div>
    </main>
  );

  // CHAT VIEW
  const code = roomInfo?.roomCode || localStorage.getItem(ROOM_CODE_KEY) || "...";
  const connectionLabel = transport === "relay" ? "Connected via relay (encrypted)" : transport === "client" ? "Connected to host" : `Host: ${roomInfo?.deviceName || "..."}`;
  const ipLine = transport === "host" ? (roomInfo?.ip ? `Your IP: ${roomInfo.ip}` : "") : transport === "client" ? (hostRef.current ? `Host: ${hostRef.current}` : "") : "";
  const privacyNote = transport === "relay"
    ? "End-to-end encrypted with your room password. Our relay only forwards ciphertext - it cannot read your messages or files."
    : "Stays on your local network. No account, no cloud.";
  const maxFileLabel = transport === "relay" ? REMOTE_MAX_LABEL : LOCAL_MAX_LABEL;
  return (
    <main className={`app-shell theme-${theme}${leftOpen ? " left-open" : ""}${rightOpen ? " right-open" : ""}`} onTouchStart={onChatTouchStart} onTouchEnd={onChatTouchEnd}>
      {(leftOpen || rightOpen) && <div className="drawer-backdrop" onClick={closeDrawers} />}
      <button type="button" className="swipe-hint swipe-hint-left" onClick={() => { setRightOpen(false); setLeftOpen(true); }} aria-label="Open links and help"><ChevronRight size={15} /></button>
      <button type="button" className="swipe-hint swipe-hint-right" onClick={() => { setLeftOpen(false); setRightOpen(true); }} aria-label="Open room and saved conversations"><ChevronLeft size={15} /></button>
      <aside className="terminal-rail" aria-label="Room info">
        <div className="brand-block"><div className="terminal-header">{BRAND_PROMPT}</div><div className="brand-title"><Terminal size={20} /><span>{APP_NAME}</span></div></div>
        <div className="connection-card">
          <div className="connection-light connected" />
          <div><strong>Room: {code}</strong><span>{connectionLabel}</span>{ipLine && <span>{ipLine}</span>}</div>
        </div>
        <div className="room-code-sidebar">
          <div className="rail-conversations-header"><Users size={14} /><span>Room Code</span></div>
          <div className="room-code-sidebar-value">{code}</div>
          <div className="room-code-note">Max file size: {maxFileLabel}</div>
          <button className="copy-code-sidebar" onClick={copyRoomCode}><Clipboard size={14} /><span>{copied ? "Copied" : "Copy"}</span></button>
        </div>
        <div className="rail-conversations">
          <div className="rail-conversations-header"><History size={14} /><span>Saved conversations</span></div>
          <div className="rail-conversations-list">
            {savedConversations.length === 0 && <div className="rail-conversations-empty">No saved conversations yet.</div>}
            {savedConversations.map(conv => (
              <div key={conv.id} className={`rail-conversation-item ${viewingSaved?.id === conv.id ? "active" : ""}`} onClick={() => handleLoadConversation(conv)}>
                <div className="rail-conversation-info"><strong>{conv.label}</strong><span>{conv.messages.length} messages</span></div>
                <button type="button" className="rail-conversation-delete" onClick={e => { e.stopPropagation(); handleDeleteConversation(conv.id); }} title="Delete"><Trash2 size={12} /></button>
              </div>
            ))}
          </div>
        </div>
        <div className="privacy-note"><ShieldCheck size={17} /><span>{privacyNote}</span></div>
      </aside>
      <section className="transfer-workspace">
        <header className="transfer-topbar">
          <div className="peer-heading">
            <button type="button" className="drawer-toggle drawer-toggle-left" onClick={() => { setRightOpen(false); setLeftOpen(true); }} aria-label="Open links and help" title="Links & help"><Menu size={20} /></button>
            <span className="status-dot linked" />
            <div><h1>{viewingSaved ? viewingSaved.label : `Room: ${code}`}</h1><p>{viewingSaved ? "Viewing saved conversation" : getDevicesCount()}</p></div>
          </div>
          <div className="top-actions">
            {!viewingSaved && <button type="button" className="save-button" onClick={handleSaveConversation} title="Save conversation"><Bookmark size={17} /><span>Save</span></button>}
            {!viewingSaved && messages.length > 0 && <button type="button" className="clear-button" onClick={handleClearMessages} title="Clear messages"><Trash2 size={17} /><span>Clear</span></button>}
            {viewingSaved && <button type="button" className="disconnect-button" onClick={handleBackToLive}><X size={17} /><span>Back to live</span></button>}
            <ThemeToggle theme={theme} onToggleTheme={onToggleTheme} />
            <button type="button" className="disconnect-button" onClick={leaveRoom}><LogOut size={17} /><span>Leave Room</span></button>
            <button type="button" className="drawer-toggle drawer-toggle-right" onClick={() => { setLeftOpen(false); setRightOpen(true); }} aria-label="Open room info" title="Room & saved"><Users size={20} /></button>
          </div>
        </header>
        <div className="workspace-grid panel-hidden">
          <MessageTranscript messages={viewingSaved ? viewingSaved.messages : messages} onSaveFile={viewingSaved || transport === "host" ? undefined : saveFile} selfName={myDeviceName || "Me"} />
        </div>
        <form className="composer" onSubmit={sendMessage}>
          <input ref={fileInputRef} type="file" multiple onChange={onFilesPicked} style={{ display: "none" }} />
          <button type="button" className="icon-button" onClick={chooseFiles} title="Send files"><Paperclip size={18} /></button>
          <input value={draft} onChange={e => setDraft(e.target.value)} placeholder="Send a message" autoComplete="off" />
          <button type="submit" className="send-button" title="Send"><Send size={18} /><span>Send</span></button>
        </form>
        <footer className="global-footer">
          <div className="footer-drawer-title">Help &amp; Links</div>
          <div className="footer-links">
            <a className="link-report" href="https://github.com/EerieGoesD/sidewire/issues/new?template=bug-report.md" target="_blank" rel="noreferrer">Report Issue</a>
            <span className="sep">|</span>
            <a className="link-feedback" href="https://github.com/EerieGoesD/sidewire/discussions" target="_blank" rel="noreferrer">Feedback</a>
            <span className="sep">|</span>
            <a className="link-feature" href="https://github.com/EerieGoesD/sidewire/issues/new?template=feature-request.md" target="_blank" rel="noreferrer">Suggest Feature</a>
            <span className="sep">|</span>
            <a className="link-coffee" href="https://buymeacoffee.com/eeriegoesd" target="_blank" rel="noreferrer">Support This Project</a>
            <span className="sep">|</span>
            <a className="link-report" href="https://eeriegoesd.com/privacy/sidewire/" target="_blank" rel="noreferrer">Privacy</a>
            <span className="sep">|</span>
            <span className="footer-madeby">Made by <a className="link-eerie" href="https://eeriegoesd.com/" target="_blank" rel="noreferrer">EERIE</a></span>
          </div>
        </footer>
      </section>
    </main>
  );
}

export default App;
