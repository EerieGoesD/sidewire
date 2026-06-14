# SideWire

Message and file transfer between your own devices - locally over Wi-Fi or remotely with end-to-end encryption. No accounts.

## How it works

SideWire has two ways to connect:

- **Local (same Wi-Fi):** the app runs a small server on your device. Other devices on the same network discover the room and connect directly. Traffic stays on your local network; nothing goes to any server of ours.
- **Remote (anywhere):** rooms are relayed through our server. Messages, files, and file names are **end-to-end encrypted on your device** with a key derived from the room password, so the relay only ever forwards ciphertext and cannot read your content. Remote rooms require a password.

A phone-accessible transfer page is also served from a Local host for browsers that do not have the app installed.

The relay is open source under `relay/` and can be self-hosted. See the [privacy policy](https://eeriegoesd.com/privacy/sidewire/) for what the relay processes.

## Run

```powershell
npm install
npm run tauri dev
```

Open the pairing URL shown in the app from a phone on the same Wi-Fi network.

## Build

```powershell
npm run tauri build
```

Tauri builds Windows EXE/MSI bundles. For Microsoft Store/MSIX, use:

```powershell
.\scripts\build-msix.ps1 `
  -IdentityName "YOUR_STORE_IDENTITY" `
  -Publisher "CN=YOUR_PUBLISHER_ID" `
  -Version "0.1.0.0" `
  -CertificateThumbprint "YOUR_CERT_THUMBPRINT"
```

The MSIX manifest template includes `internetClient`, `privateNetworkClientServer`, and `runFullTrust` because the app hosts a local LAN transfer endpoint.
