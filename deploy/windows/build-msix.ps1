$ErrorActionPreference = 'Stop'
if ($args.Count -ne 2) { throw 'usage: build-msix.ps1 RELEASE_DIR OUTPUT_MSIX' }
$release = (Resolve-Path $args[0]).Path
$output = [IO.Path]::GetFullPath($args[1])
if (!(Test-Path "$release\clipsync-client.exe")) { throw 'release directory must contain clipsync-client.exe' }
if (!(Get-Command makeappx -ErrorAction SilentlyContinue)) { throw 'makeappx.exe is required (Windows SDK)' }
$manifest = Join-Path $release 'AppxManifest.xml'
Copy-Item "$PSScriptRoot\AppxManifest.xml" $manifest -Force
makeappx pack /d $release /p $output /o
