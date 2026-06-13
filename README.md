# SideWire

Local-first message and file transfer for Windows and nearby devices.

## Current MVP

- Tauri 2 desktop app for Windows.
- Local LAN host started by the desktop app.
- Phone-accessible transfer page served from the Windows device.
- Text messages both ways.
- Phone-to-PC file uploads saved in the local incoming folder.
- PC-to-phone file shares exposed as token-protected local downloads.
- No cloud relay or external account.

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
