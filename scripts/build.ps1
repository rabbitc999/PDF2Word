param([switch]$Release)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$vendor = Join-Path $root 'vendor'
$zip = Join-Path $vendor 'pandoc.zip'
$exe = Join-Path $vendor 'pandoc.exe'
New-Item -ItemType Directory -Force $vendor | Out-Null
if (-not (Test-Path $exe)) {
  $latest = Invoke-RestMethod 'https://api.github.com/repos/jgm/pandoc/releases/latest'
  $asset = $latest.assets | Where-Object { $_.name -match 'windows-x86_64\.zip$' } | Select-Object -First 1
  if (-not $asset) { throw 'Pandoc Windows x86_64 asset not found' }
  Write-Host "Downloading Pandoc $($latest.tag_name)..."
  Invoke-WebRequest $asset.browser_download_url -OutFile $zip
  Expand-Archive $zip -DestinationPath (Join-Path $vendor 'unpack') -Force
  $found = Get-ChildItem (Join-Path $vendor 'unpack') -Filter pandoc.exe -Recurse | Select-Object -First 1
  if (-not $found) { throw 'pandoc.exe not found in archive' }
  Copy-Item $found.FullName $exe -Force
  Remove-Item $zip,(Join-Path $vendor 'unpack') -Recurse -Force
}
$cargoArgs = @('build','--bin','pdf_to_word','--bin','pdf_to_word_gui')
if ($Release) { $cargoArgs += '--release' }
Push-Location $root
try { cargo @cargoArgs } finally { Pop-Location }
