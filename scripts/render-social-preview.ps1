# Renders docs/social-preview.svg to docs/social-preview.png (1280x640) with headless Edge.
# Upload the PNG in the repository's Settings > General > Social preview.
$ErrorActionPreference = 'Stop'
$edge = @(
    "${env:ProgramFiles(x86)}\Microsoft\Edge\Application\msedge.exe",
    "$env:ProgramFiles\Microsoft\Edge\Application\msedge.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $edge) { throw 'Microsoft Edge not found' }
$docs = Resolve-Path (Join-Path $PSScriptRoot '..\docs')
$svg = Join-Path $docs 'social-preview.svg'
$png = Join-Path $docs 'social-preview.png'
$html = Join-Path ([System.IO.Path]::GetTempPath()) 'tincan-social-preview.html'
$uri = ([System.Uri]$svg).AbsoluteUri
Set-Content -Encoding utf8 $html "<!DOCTYPE html><html><body style=`"margin:0`"><img src=`"$uri`" width=`"1280`" height=`"640`" style=`"display:block`"></body></html>"
& $edge --headless=new --disable-gpu --hide-scrollbars --force-device-scale-factor=1 --window-size=1280,640 "--screenshot=$png" ([System.Uri]$html).AbsoluteUri | Out-Null
Remove-Item $html
Write-Host "saved $png"
