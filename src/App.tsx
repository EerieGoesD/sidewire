import { useCallback, useEffect, useRef, useState } from "react";
import {
  Bookmark,
  Camera,
  CheckCircle2,
  Clipboard,
  Download,
  FileText,
  History,
  Link2,
  LogOut,
  Moon,
  Monitor,
  Paperclip,
  Send,
  ShieldCheck,
  Smartphone,
  Sun,
  Terminal,
  Trash2,
  Wifi,
  X,
} from "lucide-react";
import jsQR from "jsqr";
import { QRCodeSVG } from "qrcode.react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import "./App.css";

type Sender = "pc" | "phone" | "system";
type MessageKind = "text" | "file" | "system";

type LinkMessage = {
  id: string;
  sender: Sender;
  kind: MessageKind;
  body: string;
  fileName?: string;
  fileSize?: number;
  fileMime?: string;
  downloadUrl?: string;
  createdAt: number;
};

type LinkProfile = {
  deviceName: string;
  platform: string;
  arch: string;
  primaryAddress: string;
  pairingCode: string;
  pairingUrl: string;
  sessionPort: number;
  protocol: string;
  incomingDir: string;
  phoneConnected: boolean;
};

type SavedConversation = {
  id: string;
  label: string;
  savedAt: number;
  messages: LinkMessage[];
};

const fallbackProfile: LinkProfile = {
  deviceName: "This PC",
  platform: "preview",
  arch: "web",
  primaryAddress: "127.0.0.1",
  pairingCode: "",
  pairingUrl: "",
  sessionPort: 0,
  protocol: "preview",
  incomingDir: "",
  phoneConnected: false,
};

const APP_NAME = "SideWire";
const BRAND_PROMPT = "sidewire";
const THEME_STORAGE_KEY = "sidewire-theme";
const LEGACY_THEME_STORAGE_KEY = "device-bridge-theme";
const INVITE_STORAGE_KEY = "sidewire-invite";
const LEGACY_INVITE_STORAGE_KEYS = ["device-bridge-invite", "eerie-link-invite"];

type ThemeMode = "dark" | "light";

type AppViewProps = {
  theme: ThemeMode;
  onToggleTheme: () => void;
};

function readTheme(): ThemeMode {
  return localStorage.getItem(THEME_STORAGE_KEY) === "light" ||
    localStorage.getItem(LEGACY_THEME_STORAGE_KEY) === "light"
    ? "light"
    : "dark";
}

function readStoredInvite() {
  const value =
    localStorage.getItem(INVITE_STORAGE_KEY) ||
    LEGACY_INVITE_STORAGE_KEYS.map((key) => localStorage.getItem(key) || "").find(Boolean) ||
    "";
  try {
    const parsed = new URL(value);
    return parsed.protocol === "http:" && parsed.searchParams.has("token") ? value : "";
  } catch {
    return "";
  }
}

function clearStoredInvite() {
  [INVITE_STORAGE_KEY, ...LEGACY_INVITE_STORAGE_KEYS].forEach((key) => localStorage.removeItem(key));
}

function friendlyConnectionError(err: unknown) {
  const message = err instanceof Error ? err.message : String(err);
  if (message === "Failed to fetch" || message.includes("NetworkError")) {
    return "Could not connect to the Windows app. Keep both devices on the same Wi-Fi and allow SideWire through Windows Firewall.";
  }
  return message;
}

function formatBytes(bytes?: number) {
  if (!bytes) return "0 B";
  const units = ["B", "KB", "MB", "GB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(value >= 10 || unit === 0 ? 0 : 1)} ${units[unit]}`;
}

function formatTime(value: number) {
  return new Date(value).toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
  });
}

function senderTitle(sender: Sender) {
  if (sender === "pc") return "This PC";
  if (sender === "phone") return "Phone";
  return APP_NAME;
}

function App() {
  const [theme, setTheme] = useState<ThemeMode>(readTheme);

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
  }, [theme]);

  function toggleTheme() {
    setTheme((current) => {
      const next = current === "dark" ? "light" : "dark";
      localStorage.setItem(THEME_STORAGE_KEY, next);
      localStorage.removeItem(LEGACY_THEME_STORAGE_KEY);
      return next;
    });
  }

  return /Android/i.test(navigator.userAgent) ? (
    <PhoneClientApp theme={theme} onToggleTheme={toggleTheme} />
  ) : (
    <DesktopApp theme={theme} onToggleTheme={toggleTheme} />
  );
}

function ThemeToggle({ theme, onToggleTheme }: AppViewProps) {
  const Icon = theme === "dark" ? Sun : Moon;

  return (
    <button type="button" className="theme-toggle" onClick={onToggleTheme} title="Switch theme">
      <Icon size={17} />
      <span>{theme === "dark" ? "Light" : "Dark"}</span>
    </button>
  );
}

function MessageTranscript({
  messages,
  downloadHref,
}: {
  messages: LinkMessage[];
  downloadHref: (msg: LinkMessage) => string;
}) {
  const transcriptRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    transcriptRef.current?.scrollTo({
      top: transcriptRef.current.scrollHeight,
      behavior: "smooth",
    });
  }, [messages.length]);

  return (
    <div className="transcript" ref={transcriptRef}>
      {messages.map((message) => (
        <article
          className={`message ${message.sender} ${
            message.sender === "system"
              ? message.body === "Phone connected."
                ? "connected"
                : message.body.startsWith("Phone disconnected")
                ? "disconnected"
                : ""
              : ""
          }`}
          key={message.id}
        >
          <div className="message-meta">
            <span>{senderTitle(message.sender)}</span>
            <time>{formatTime(message.createdAt)}</time>
          </div>
          <p>{message.body}</p>
          {message.kind === "file" && (
            <div className="file-card">
              <FileText size={20} />
              <div>
                <strong>{message.fileName}</strong>
                <span>{formatBytes(message.fileSize)}</span>
              </div>
              {message.downloadUrl && (
                <a href={downloadHref(message)} target="_blank" rel="noreferrer" title="Download file">
                  <Download size={17} />
                </a>
              )}
            </div>
          )}
        </article>
      ))}
    </div>
  );
}

function DesktopApp({ theme, onToggleTheme }: AppViewProps) {
  const [profile, setProfile] = useState<LinkProfile>(fallbackProfile);
  const [messages, setMessages] = useState<LinkMessage[]>([]);
  const [draft, setDraft] = useState("");
  const [copied, setCopied] = useState(false);
  const [error, setError] = useState("");

  // Conversation history (displayed in the left sidebar rail)
  const [savedConversations, setSavedConversations] = useState<SavedConversation[]>([]);
  const [viewingSaved, setViewingSaved] = useState<SavedConversation | null>(null);
  const [connectPanelOpen, setConnectPanelOpen] = useState(true);

  const loadProfile = useCallback(async () => {
    try {
      setProfile(await invoke<LinkProfile>("get_link_profile"));
      setError("");
    } catch (err) {
      setProfile(fallbackProfile);
      setError(String(err));
    }
  }, []);

  const refreshMessages = useCallback(async () => {
    try {
      setMessages(await invoke<LinkMessage[]>("list_messages"));
      setError("");
    } catch (err) {
      setError(String(err));
    }
  }, []);

  const refreshSavedConversations = useCallback(async () => {
    try {
      setSavedConversations(await invoke<SavedConversation[]>("list_saved_conversations"));
    } catch {
      // Ignore — feature not available
    }
  }, []);

  useEffect(() => {
    loadProfile();
    refreshMessages();
    refreshSavedConversations();
    const id = window.setInterval(() => {
      loadProfile();
      refreshMessages();
    }, 1200);
    return () => window.clearInterval(id);
  }, [loadProfile, refreshMessages, refreshSavedConversations]);

  async function sendMessage(event: React.FormEvent) {
    event.preventDefault();
    const text = draft.trim();
    if (!text) return;
    setDraft("");

    try {
      await invoke<LinkMessage>("send_text", { text });
      await refreshMessages();
    } catch (err) {
      setError(String(err));
    }
  }

  async function chooseFiles() {
    try {
      const selected = await open({
        multiple: true,
        directory: false,
        title: "Send files to your phone",
      });
      const paths = Array.isArray(selected) ? selected : selected ? [selected] : [];
      for (const path of paths) {
        await invoke<LinkMessage>("share_file", { path });
      }
      await refreshMessages();
    } catch (err) {
      setError(String(err));
    }
  }

  async function copyInvite() {
    try {
      await navigator.clipboard.writeText(profile.pairingUrl);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1400);
    } catch (err) {
      setError(String(err));
    }
  }

  async function disconnectPhone() {
    try {
      await invoke<LinkMessage>("disconnect_phone");
      await loadProfile();
      await refreshMessages();
    } catch (err) {
      setError(String(err));
    }
  }

  async function handleSaveConversation() {
    try {
      await invoke<SavedConversation>("save_conversation", { label: "" });
      await refreshSavedConversations();
    } catch (err) {
      setError(String(err));
    }
  }

  async function handleLoadConversation(conv: SavedConversation) {
    try {
      const loaded = await invoke<SavedConversation>("load_conversation", { id: conv.id });
      setViewingSaved(loaded);
    } catch (err) {
      setError(String(err));
    }
  }

  async function handleDeleteConversation(id: string) {
    try {
      await invoke<void>("delete_conversation", { id });
      if (viewingSaved?.id === id) {
        setViewingSaved(null);
      }
      await refreshSavedConversations();
    } catch (err) {
      setError(String(err));
    }
  }

  function handleBackToLive() {
    setViewingSaved(null);
  }

  function downloadHref(message: LinkMessage) {
    if (!message.downloadUrl) return "";
    const raw = message.downloadUrl.startsWith("http")
      ? message.downloadUrl
      : `http://${profile.primaryAddress}:${profile.sessionPort}${message.downloadUrl}`;
    try {
      const url = new URL(raw);
      url.searchParams.set("token", profile.pairingCode);
      return url.toString();
    } catch {
      return raw;
    }
  }

  return (
    <main className={`app-shell theme-${theme}`}>
      <aside className="terminal-rail" aria-label="Connection status">
        <div className="brand-block">
          <div className="terminal-header">{BRAND_PROMPT}</div>
          <div className="brand-title">
            <Terminal size={20} />
            <span>{APP_NAME}</span>
          </div>
        </div>

        <div className="connection-card">
          <div className={`connection-light ${profile.phoneConnected ? "connected" : ""}`} />
          <div>
            <strong>{profile.phoneConnected ? "Phone connected" : "Waiting for phone"}</strong>
            <span>
              {profile.phoneConnected
                ? "You can send messages and files now."
                : "Use the invite on the right to connect."}
            </span>
          </div>
        </div>

        <div className="device-list">
          <div className="device-row">
            <Monitor size={18} />
            <div>
              <strong>{profile.deviceName || "This PC"}</strong>
              <span>Ready to share</span>
            </div>
            <CheckCircle2 size={16} />
          </div>
          <div className="device-row">
            <Smartphone size={18} />
            <div>
              <strong>Your phone</strong>
              <span>{profile.phoneConnected ? "Connected" : "Not connected yet"}</span>
            </div>
            <span className="small-state">{profile.phoneConnected ? "online" : "wait"}</span>
          </div>
        </div>

        {/* Saved conversations integrated into the sidebar rail */}
        <div className="rail-conversations">
          <div className="rail-conversations-header">
            <History size={14} />
            <span>Saved conversations</span>
          </div>
          <div className="rail-conversations-list">
            {savedConversations.length === 0 && (
              <div className="rail-conversations-empty">No saved conversations yet.</div>
            )}
            {savedConversations.map((conv) => (
              <div
                key={conv.id}
                className={`rail-conversation-item ${viewingSaved?.id === conv.id ? "active" : ""}`}
                onClick={() => handleLoadConversation(conv)}
              >
                <div className="rail-conversation-info">
                  <strong>{conv.label}</strong>
                  <span>{conv.messages.length} messages</span>
                </div>
                <button
                  type="button"
                  className="rail-conversation-delete"
                  onClick={(e) => {
                    e.stopPropagation();
                    handleDeleteConversation(conv.id);
                  }}
                  title="Delete conversation"
                >
                  <Trash2 size={12} />
                </button>
              </div>
            ))}
          </div>
        </div>

        <div className="privacy-note">
          <ShieldCheck size={17} />
          <span>Encrypted. No account, no cloud relay. The invite only works from nearby devices on your network.</span>
        </div>
      </aside>

      <section className="transfer-workspace">
        <header className="transfer-topbar">
          <div className="peer-heading">
            <span className={`status-dot ${profile.phoneConnected ? "linked" : "waiting"}`} />
            <div>
              <h1>{viewingSaved ? viewingSaved.label : "Device transfer"}</h1>
              <p>{viewingSaved ? "Viewing saved conversation" : profile.phoneConnected ? "Connected to your phone" : "Connect your phone to send messages/files"}</p>
            </div>
          </div>
          <div className="top-actions">
            {!viewingSaved && (
              <button type="button" className="save-button" onClick={handleSaveConversation} title="Save this conversation">
                <Bookmark size={17} />
                <span>Save</span>
              </button>
            )}
            {viewingSaved && (
              <button type="button" className="disconnect-button" onClick={handleBackToLive}>
                <X size={17} />
                <span>Back to live</span>
              </button>
            )}
            <button
              type="button"
              className={`panel-toggle ${connectPanelOpen ? "active" : ""}`}
              onClick={() => setConnectPanelOpen(!connectPanelOpen)}
              title={connectPanelOpen ? "Hide connect panel" : "Show connect panel"}
            >
              <Link2 size={17} />
            </button>
            <ThemeToggle theme={theme} onToggleTheme={onToggleTheme} />
            {profile.phoneConnected && (
              <button type="button" className="disconnect-button" onClick={disconnectPhone}>
                <LogOut size={17} />
                <span>Disconnect</span>
              </button>
            )}
            <button type="button" className="invite-button" onClick={copyInvite}>
              <Clipboard size={17} />
              <span>{copied ? "Invite copied" : "Copy invite"}</span>
            </button>
          </div>
        </header>

        <div className={`workspace-grid ${connectPanelOpen ? "" : "panel-hidden"}`}>
          <MessageTranscript
            messages={viewingSaved ? viewingSaved.messages : messages}
            downloadHref={downloadHref}
          />

          {connectPanelOpen && (
          <aside className="connect-panel" aria-label="Connect a phone">
            <div className="panel-command">
              <Link2 size={18} />
              <span>Connect your phone</span>
              <button
                type="button"
                className="panel-close"
                onClick={() => setConnectPanelOpen(false)}
                title="Hide panel"
              >
                <X size={15} />
              </button>
            </div>
            <div className="qr-shell">
              {profile.pairingUrl ? (
                <QRCodeSVG
                  value={profile.pairingUrl}
                  size={188}
                  bgColor={theme === "dark" ? "#050508" : "#f7f8fb"}
                  fgColor={theme === "dark" ? "#1e90ff" : "#135da8"}
                  level="M"
                  marginSize={2}
                />
              ) : (
                <div className="qr-placeholder">Starting invite</div>
              )}
            </div>
            <div className="connect-steps">
              <div>
                <Wifi size={16} />
                <span>Keep both devices on the same Wi-Fi.</span>
              </div>
              <div>
                <Smartphone size={16} />
                <span>Scan this code with your phone camera.</span>
              </div>
              <div>
                <Paperclip size={16} />
                <span>Send messages or files from either side.</span>
              </div>
            </div>
            <button type="button" className="copy-link-button" onClick={copyInvite}>
              <Clipboard size={17} />
              <span>{copied ? "Copied" : "Copy invite link"}</span>
            </button>
            {error && <div className="error-line">{error}</div>}
          </aside>
          )}
        </div>

        <form className="composer" onSubmit={sendMessage}>
          <button type="button" className="icon-button" onClick={chooseFiles} title="Send files">
            <Paperclip size={18} />
          </button>
          <input
            value={draft}
            onChange={(event) => setDraft(event.currentTarget.value)}
            placeholder="Send a message to your phone"
            autoComplete="off"
          />
          <button type="submit" className="send-button" title="Send message">
            <Send size={18} />
            <span>Send</span>
          </button>
        </form>

        <footer className="global-footer">
          <div className="footer-links">
            <a className="link-report" href="https://github.com/EerieGoesD/sidewire/issues/new?template=bug-report.md" target="_blank" rel="noreferrer">Report Issue</a>
            <span className="sep">|</span>
            <a className="link-feedback" href="https://github.com/EerieGoesD/sidewire/discussions" target="_blank" rel="noreferrer">Feedback</a>
            <span className="sep">|</span>
            <a className="link-feature" href="https://github.com/EerieGoesD/sidewire/issues/new?template=feature-request.md" target="_blank" rel="noreferrer">Suggest Feature</a>
            <span className="sep">|</span>
            <a className="link-coffee" href="https://buymeacoffee.com/eeriegoesd" target="_blank" rel="noreferrer">Support This Project</a>
            <span className="sep">|</span>
            <a className="link-eerie" href="https://eeriegoesd.com/" target="_blank" rel="noreferrer">EERIE</a>
          </div>
        </footer>
      </section>
    </main>
  );
}

function PhoneClientApp({ theme, onToggleTheme }: AppViewProps) {
  const [invite, setInvite] = useState(readStoredInvite);
  const [connected, setConnected] = useState(false);
  const [scanning, setScanning] = useState(false);
  const [messages, setMessages] = useState<LinkMessage[]>([]);
  const [draft, setDraft] = useState("");
  const [error, setError] = useState("");
  const transcriptRef = useRef<HTMLDivElement>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const videoRef = useRef<HTMLVideoElement>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const streamRef = useRef<MediaStream | null>(null);
  const scanFrameRef = useRef<number | null>(null);
  const manualDisconnectRef = useRef(false);

  const inviteParts = useCallback(() => {
    const parsed = new URL(invite.trim());
    const token = parsed.searchParams.get("token");
    if (!token) throw new Error("That invite link is not valid.");
    return { base: parsed.origin, token };
  }, [invite]);

  const api = useCallback(
    (path: string) => {
      const { base, token } = inviteParts();
      return `${base}${path}${path.includes("?") ? "&" : "?"}token=${encodeURIComponent(token)}`;
    },
    [inviteParts],
  );

  const refreshInvite = useCallback(async (nextInvite: string) => {
    if (manualDisconnectRef.current) return false;
    if (!nextInvite.trim()) return false;
    try {
      const parsed = new URL(nextInvite.trim());
      if (parsed.protocol !== "http:") {
        throw new Error("That QR code is not a SideWire invite.");
      }
      const token = parsed.searchParams.get("token");
      if (!token) throw new Error("That invite link is not valid.");
      const response = await fetch(`${parsed.origin}/api/messages?token=${encodeURIComponent(token)}`, {
        cache: "no-store",
      });
      if (response.status === 401) {
        clearStoredInvite();
        setInvite("");
        setMessages([]);
        throw new Error("This invite was disconnected. Scan the new QR code again.");
      }
      if (!response.ok) throw new Error("Could not reach the PC.");
      const nextMessages = await response.json();
      if (manualDisconnectRef.current) return false;
      setMessages(nextMessages);
      setConnected(true);
      setError("");
      localStorage.setItem(INVITE_STORAGE_KEY, nextInvite.trim());
      return true;
    } catch (err) {
      setConnected(false);
      setError(friendlyConnectionError(err));
      return false;
    }
  }, []);

  const refresh = useCallback(async () => {
    await refreshInvite(invite);
  }, [invite, refreshInvite]);

  useEffect(() => {
    refresh();
    const id = window.setInterval(refresh, 1200);
    return () => window.clearInterval(id);
  }, [refresh]);

  useEffect(() => {
    transcriptRef.current?.scrollTo({
      top: transcriptRef.current.scrollHeight,
      behavior: "smooth",
    });
  }, [messages.length]);

  useEffect(() => {
    return () => {
      stopQrScanner();
    };
  }, []);

  async function connect(event: React.FormEvent) {
    event.preventDefault();
    manualDisconnectRef.current = false;
    await refreshInvite(invite);
  }

  async function sendMessage(event: React.FormEvent) {
    event.preventDefault();
    const text = draft.trim();
    if (!text) return;
    setDraft("");
    try {
      const response = await fetch(api("/api/message"), { method: "POST", body: text });
      if (!response.ok) throw new Error("Message could not be sent.");
      await refresh();
    } catch (err) {
      setError(String(err instanceof Error ? err.message : err));
    }
  }

  async function uploadFiles(files: FileList | null) {
    if (!files?.length) return;
    try {
      for (const file of Array.from(files)) {
        const target =
          api("/api/upload") +
          `&name=${encodeURIComponent(file.name)}&mime=${encodeURIComponent(file.type || "application/octet-stream")}`;
        const response = await fetch(target, { method: "POST", body: file });
        if (!response.ok) throw new Error(`${file.name} could not be sent.`);
      }
      await refresh();
    } catch (err) {
      setError(String(err instanceof Error ? err.message : err));
    } finally {
      if (fileInputRef.current) fileInputRef.current.value = "";
    }
  }

  async function disconnectFromPc() {
    manualDisconnectRef.current = true;
    setError("");

    try {
      if (invite.trim()) {
        await fetch(api("/api/disconnect"), { method: "POST" });
      }
    } catch {
      // Local disconnect should still clear the phone UI even if the PC is already gone.
    }

    stopQrScanner();
    clearStoredInvite();
    setInvite("");
    setConnected(false);
    setMessages([]);
    setDraft("");
  }

  function stopQrScanner() {
    if (scanFrameRef.current !== null) {
      window.cancelAnimationFrame(scanFrameRef.current);
      scanFrameRef.current = null;
    }

    streamRef.current?.getTracks().forEach((track) => track.stop());
    streamRef.current = null;

    if (videoRef.current) {
      videoRef.current.srcObject = null;
    }

    setScanning(false);
  }

  async function handleScannedInvite(value: string) {
    const parsed = new URL(value);
    if (parsed.protocol !== "http:") {
      throw new Error("That QR code is not a SideWire invite.");
    }
    if (!parsed.searchParams.get("token")) {
      throw new Error("That QR code is not a device invite.");
    }
    const nextInvite = parsed.toString();
    manualDisconnectRef.current = false;
    const successful = await refreshInvite(nextInvite);
    if (successful) {
      setInvite(nextInvite);
      stopQrScanner();
    }
  }

  function scanVideoFrame() {
    const video = videoRef.current;
    const canvas = canvasRef.current;
    const context = canvas?.getContext("2d", { willReadFrequently: true });

    if (!video || !canvas || !context) {
      scanFrameRef.current = window.requestAnimationFrame(scanVideoFrame);
      return;
    }

    if (video.readyState === video.HAVE_ENOUGH_DATA && video.videoWidth > 0 && video.videoHeight > 0) {
      canvas.width = video.videoWidth;
      canvas.height = video.videoHeight;
      context.drawImage(video, 0, 0, canvas.width, canvas.height);
      const imageData = context.getImageData(0, 0, canvas.width, canvas.height);
      const result = jsQR(imageData.data, imageData.width, imageData.height);

      if (result?.data) {
        handleScannedInvite(result.data).catch((err) => {
          setError(String(err instanceof Error ? err.message : err));
          scanFrameRef.current = window.requestAnimationFrame(scanVideoFrame);
        });
        return;
      }
    }

    scanFrameRef.current = window.requestAnimationFrame(scanVideoFrame);
  }

  async function startQrScanner() {
    if (!navigator.mediaDevices?.getUserMedia) {
      setError("Camera scanning is not available on this device.");
      return;
    }

    try {
      setError("");
      setScanning(true);

      const stream = await navigator.mediaDevices.getUserMedia({
        video: {
          facingMode: { ideal: "environment" },
          width: { ideal: 1280 },
          height: { ideal: 720 },
        },
        audio: false,
      });

      streamRef.current = stream;

      if (!videoRef.current) {
        stream.getTracks().forEach((track) => track.stop());
        return;
      }

      videoRef.current.srcObject = stream;
      await videoRef.current.play();
      scanFrameRef.current = window.requestAnimationFrame(scanVideoFrame);
    } catch (err) {
      stopQrScanner();
      setError(String(err instanceof Error ? err.message : err));
    }
  }

  function downloadHref(message: LinkMessage) {
    if (!message.downloadUrl) return "";
    try {
      const { base, token } = inviteParts();
      const raw = message.downloadUrl.startsWith("http") ? message.downloadUrl : `${base}${message.downloadUrl}`;
      const url = new URL(raw);
      url.searchParams.set("token", token);
      return url.toString();
    } catch {
      return message.downloadUrl;
    }
  }

  return (
    <main className={`phone-shell theme-${theme}`}>
      <header className="phone-header">
        <div>
          <div className="terminal-header">{BRAND_PROMPT}</div>
          <h1>Connect to your PC</h1>
        </div>
        <div className="phone-header-actions">
          <ThemeToggle theme={theme} onToggleTheme={onToggleTheme} />
          {connected ? (
            <button type="button" className="disconnect-button phone-disconnect" onClick={disconnectFromPc}>
              <LogOut size={16} />
              <span>Disconnect</span>
            </button>
          ) : (
            <span className="phone-state">Waiting</span>
          )}
        </div>
      </header>

      {!connected && (
        <form className="invite-form" onSubmit={connect}>
          <label htmlFor="invite">Scan the QR code shown on your PC</label>
          <button type="button" className="scan-button" onClick={startQrScanner}>
            <Camera size={18} />
            <span>Scan QR code</span>
          </button>
          <div className="invite-separator">or paste the invite link</div>
          <input
            id="invite"
            value={invite}
            onChange={(event) => setInvite(event.currentTarget.value)}
            placeholder="Paste invite link"
            autoComplete="off"
          />
          <button type="submit">
            <Link2 size={17} />
            <span>Connect</span>
          </button>
          <p>Open the Windows app first. The QR code and invite are shown on the Connect your phone panel.</p>
        </form>
      )}

      <section className="phone-transfer" ref={transcriptRef}>
        {messages.map((message) => (
          <article className={`message ${message.sender}`} key={message.id}>
            <div className="message-meta">
              <span>{senderTitle(message.sender)}</span>
              <time>{formatTime(message.createdAt)}</time>
            </div>
            <p>{message.body}</p>
            {message.kind === "file" && (
              <div className="file-card">
                <FileText size={20} />
                <div>
                  <strong>{message.fileName}</strong>
                  <span>{formatBytes(message.fileSize)}</span>
                </div>
                {message.downloadUrl && (
                  <a href={downloadHref(message)} target="_blank" rel="noreferrer" title="Download file">
                    <Download size={17} />
                  </a>
                )}
              </div>
            )}
          </article>
        ))}
      </section>

      {error && <div className="phone-error">{error}</div>}

      <form className="composer phone-composer" onSubmit={sendMessage}>
        <button type="button" className="icon-button" onClick={() => fileInputRef.current?.click()} title="Send files">
          <Paperclip size={18} />
        </button>
        <input
          ref={fileInputRef}
          className="hidden-file"
          type="file"
          multiple
          onChange={(event) => uploadFiles(event.currentTarget.files)}
        />
        <input
          value={draft}
          onChange={(event) => setDraft(event.currentTarget.value)}
          placeholder={connected ? "Send a message to your PC" : "Connect first"}
          autoComplete="off"
          disabled={!connected}
        />
        <button type="submit" className="send-button" title="Send message" disabled={!connected}>
          <Send size={18} />
          <span>Send</span>
        </button>
      </form>

      {scanning && (
        <div className="scanner-overlay" role="dialog" aria-modal="true" aria-label="Scan QR code">
          <div className="scanner-panel">
            <div className="scanner-topbar">
              <strong>Scan the code on your PC</strong>
              <button type="button" onClick={stopQrScanner}>
                Cancel
              </button>
            </div>
            <div className="scanner-view">
              <video ref={videoRef} playsInline muted />
              <canvas ref={canvasRef} aria-hidden="true" />
              <div className="scanner-frame" />
            </div>
            <p>Point your phone at the QR code in the Windows app.</p>
          </div>
        </div>
      )}
    </main>
  );
}

export default App;