#Requires -Version 5.1
# everything233 one-click install: latest release MSI from GitHub + user PATH.
# Usage:  irm https://raw.githubusercontent.com/neko233-com/everything233/main/install.ps1 | iex
$ErrorActionPreference = 'Stop'
$repo = 'neko233-com/everything233'
$ua = @{ 'User-Agent' = 'everything233-install' }

$rel = Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest" -Headers $ua
$asset = $rel.assets | Where-Object { $_.name -like '*.msi' } | Select-Object -First 1
if (-not $asset) { throw "no MSI asset in latest release" }

$msi = Join-Path $env:TEMP $asset.name
Write-Host "downloading $($asset.name) ..."
Invoke-WebRequest $asset.browser_download_url -OutFile $msi -Headers $ua

$log = Join-Path $env:TEMP 'everything233-install.log'
Write-Host "installing (UAC prompt for Program Files) ..."
Start-Process msiexec.exe -Verb RunAs -Wait -ArgumentList "/i `"$msi`" /qn /norestart /L*v `"$log`""

$installDir = Join-Path $env:ProgramFiles 'everything233'
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($userPath -split ';') -notcontains $installDir) {
    [Environment]::SetEnvironmentVariable('Path', ($userPath.TrimEnd(';') + ";$installDir"), 'User')
}

# register logon auto-start and launch daemon now
$exe = Join-Path $installDir 'everything.exe'
if (Test-Path $exe) {
    & $exe install
    & $exe serve --background
}

Write-Host "done. Open a new terminal and use:  everything <query>"
