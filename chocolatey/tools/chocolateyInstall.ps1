$ErrorActionPreference = 'Stop'

$packageName = 'aria2-rust'
$toolsDir = Split-Path -Parent $MyInvocation.MyCommand.Definition
$url = 'https://github.com/balovess/aria2_rust/releases/download/v0.3.7/aria2-x86_64-windows-full.zip'
$checksum = 'PLACEHOLDER_SHA256'

Install-ChocolateyZipPackage `
    -PackageName $packageName `
    -Url $url `
    -UnzipLocation $toolsDir `
    -Checksum $checksum `
    -ChecksumType 'sha256'

Install-ChocolateyPath -PathToInstall $toolsDir -PathType 'Machine'
