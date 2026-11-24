# Downloads the DPDFNet 48 kHz streaming model (Apache-2.0, https://huggingface.co/Ceva-IP/DPDFNet)
# into crates/tincan-audio/models/, where tincan-audio's build.rs embeds it. The file is not in git.
# Without it the app builds and runs with WebRTC noise suppression only.
$ErrorActionPreference = 'Stop'
$revision = 'dd6818d00f50c836fed43a6243ebe49116de5964'
$sha256 = '7f0575a5cec0ba4ffd8f8bd657e06d007e4ccdd955d76faab922b9d3291dc14b'
$name = 'dpdfnet2_48khz_hr.onnx'
$dir = Join-Path $PSScriptRoot '..\crates\tincan-audio\models'
$dest = Join-Path $dir $name
New-Item -ItemType Directory -Force $dir | Out-Null
if ((Test-Path $dest) -and ((Get-FileHash $dest -Algorithm SHA256).Hash -eq $sha256)) {
    Write-Host "$name is already there"
    exit 0
}
$url = "https://huggingface.co/Ceva-IP/DPDFNet/resolve/$revision/onnx/${name}?download=true"
$tmp = "$dest.part"
Invoke-WebRequest -Uri $url -OutFile $tmp -UseBasicParsing
$got = (Get-FileHash $tmp -Algorithm SHA256).Hash
if ($got -ne $sha256) {
    Remove-Item $tmp
    throw "sha256 mismatch for ${name}: $got"
}
Move-Item -Force $tmp $dest
Write-Host "saved $dest"
