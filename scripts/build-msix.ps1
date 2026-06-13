param(
  [string]$IdentityName = "EERIE.EerieLink",
  [string]$Publisher = "CN=EERIE",
  [string]$Version = "0.1.0.0",
  [ValidateSet("x64", "x86", "arm64")]
  [string]$Architecture = "x64",
  [string]$CertificateThumbprint = ""
)

$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$Root = Split-Path -Parent $ScriptDir
$ReleaseExe = Join-Path $Root "src-tauri\target\release\eerie-link.exe"
$Template = Join-Path $Root "packaging\msix\AppxManifest.xml.template"
$DistRoot = Join-Path $Root "msix-dist"
$LayoutDir = Join-Path $DistRoot "layout"
$AssetsDir = Join-Path $LayoutDir "Assets"
$PackagePath = Join-Path $DistRoot "SideWire_$($Version)_$($Architecture).msix"

function Resolve-WithinRoot {
  param([string]$Path)
  $rootFull = [System.IO.Path]::GetFullPath($Root)
  $pathFull = [System.IO.Path]::GetFullPath($Path)
  if (-not $pathFull.StartsWith($rootFull, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "Refusing to operate outside project root: $pathFull"
  }
  return $pathFull
}

function Find-WindowsKitTool {
  param([string]$ToolName)
  $kitsRoot = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\bin"
  if (-not (Test-Path $kitsRoot)) {
    throw "Windows Kits folder not found. Install Windows SDK with MSIX Packaging Tools."
  }

  $versions = Get-ChildItem -LiteralPath $kitsRoot -Directory | Sort-Object Name -Descending
  foreach ($version in $versions) {
    $candidate = Join-Path $version.FullName "$Architecture\$ToolName"
    if (Test-Path $candidate) {
      return $candidate
    }
  }

  throw "$ToolName not found under $kitsRoot"
}

function Copy-Asset {
  param(
    [string]$SourceName,
    [string]$TargetName
  )
  $source = Join-Path $Root "src-tauri\icons\$SourceName"
  $target = Join-Path $AssetsDir $TargetName
  if (Test-Path $source) {
    Copy-Item -LiteralPath $source -Destination $target -Force
  } else {
    Copy-Item -LiteralPath (Join-Path $Root "src-tauri\icons\icon.png") -Destination $target -Force
  }
}

function New-WideLogo {
  Add-Type -AssemblyName System.Drawing
  $target = Join-Path $AssetsDir "Wide310x150Logo.png"
  $source = Join-Path $Root "src-tauri\icons\icon.png"
  $bitmap = New-Object System.Drawing.Bitmap 310, 150, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
  $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
  $graphics.Clear([System.Drawing.Color]::Transparent)
  $graphics.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
  $icon = [System.Drawing.Image]::FromFile($source)
  try {
    $size = 96
    $x = [int]((310 - $size) / 2)
    $y = [int]((150 - $size) / 2)
    $graphics.DrawImage($icon, $x, $y, $size, $size)
    $bitmap.Save($target, [System.Drawing.Imaging.ImageFormat]::Png)
  } finally {
    $icon.Dispose()
    $graphics.Dispose()
    $bitmap.Dispose()
  }
}

if (-not (Test-Path $ReleaseExe)) {
  Push-Location $Root
  try {
    npm run tauri build -- --no-bundle
  } finally {
    Pop-Location
  }
}

$DistRoot = Resolve-WithinRoot $DistRoot
$LayoutDir = Resolve-WithinRoot $LayoutDir
$AssetsDir = Resolve-WithinRoot $AssetsDir

if (Test-Path $LayoutDir) {
  Remove-Item -LiteralPath $LayoutDir -Recurse -Force
}
New-Item -ItemType Directory -Force -Path $AssetsDir | Out-Null

Copy-Item -LiteralPath $ReleaseExe -Destination (Join-Path $LayoutDir "eerie-link.exe") -Force
Copy-Asset "StoreLogo.png" "StoreLogo.png"
Copy-Asset "Square44x44Logo.png" "Square44x44Logo.png"
Copy-Asset "Square150x150Logo.png" "Square150x150Logo.png"
Copy-Asset "Square310x310Logo.png" "Square310x310Logo.png"
New-WideLogo

$manifest = Get-Content -Raw -LiteralPath $Template
$manifest = $manifest.Replace("{{IDENTITY_NAME}}", $IdentityName)
$manifest = $manifest.Replace("{{PUBLISHER}}", $Publisher)
$manifest = $manifest.Replace("{{VERSION}}", $Version)
$manifest = $manifest.Replace("{{ARCHITECTURE}}", $Architecture)
Set-Content -LiteralPath (Join-Path $LayoutDir "AppxManifest.xml") -Value $manifest -Encoding UTF8

New-Item -ItemType Directory -Force -Path $DistRoot | Out-Null
$makeAppx = Find-WindowsKitTool "makeappx.exe"
& $makeAppx pack /d $LayoutDir /p $PackagePath /o /v
if ($LASTEXITCODE -ne 0) {
  throw "makeappx failed with exit code $LASTEXITCODE"
}

if ($CertificateThumbprint.Trim()) {
  $signtool = Find-WindowsKitTool "signtool.exe"
  & $signtool sign /fd SHA256 /sha1 $CertificateThumbprint $PackagePath
  if ($LASTEXITCODE -ne 0) {
    throw "signtool failed with exit code $LASTEXITCODE"
  }
}

Write-Host "MSIX created: $PackagePath"
