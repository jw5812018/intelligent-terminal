param(
    [string]$PfxPath = 'src\cascadia\CascadiaPackage\CascadiaPackage_TemporaryKey.pfx',
    [string]$CerPath = 'artifacts\local-installer\AgenticTerminalDev.cer',
    [string]$Subject = 'CN=Agentic Terminal Dev',
    [string]$FriendlyName = 'Agentic Terminal Dev'
)

$ErrorActionPreference = 'Stop'

$cert = New-SelfSignedCertificate `
    -Type Custom `
    -Subject $Subject `
    -KeyUsage DigitalSignature `
    -FriendlyName $FriendlyName `
    -CertStoreLocation 'Cert:\CurrentUser\My' `
    -TextExtension @('2.5.29.37={text}1.3.6.1.5.5.7.3.3', '2.5.29.19={text}')

Write-Host "Thumbprint: $($cert.Thumbprint)"
Write-Host "Subject:    $($cert.Subject)"
Write-Host "NotAfter:   $($cert.NotAfter)"

$emptyPwd = New-Object System.Security.SecureString

New-Item -ItemType Directory -Path (Split-Path $PfxPath -Parent) -Force | Out-Null
New-Item -ItemType Directory -Path (Split-Path $CerPath -Parent) -Force | Out-Null

Export-PfxCertificate -Cert $cert -FilePath $PfxPath -Password $emptyPwd | Out-Null
Export-Certificate    -Cert $cert -FilePath $CerPath | Out-Null

Write-Host "PFX: $PfxPath ($((Get-Item $PfxPath).Length) bytes)"
Write-Host "CER: $CerPath ($((Get-Item $CerPath).Length) bytes)"
