<#
.SYNOPSIS
    Gera o instalador: build de release e Inno Setup.

.DESCRIPTION
    Saída em dist\ProcessMonitor-Setup-<versão>.exe, com a versão lida do Cargo.toml.
    Requer Inno Setup 6 (winget install JRSoftware.InnoSetup).
    Feche o Process Monitor antes: com ele aberto o cargo não consegue
    sobrescrever target\release\process-monitor.exe.
#>
[CmdletBinding()]
param()

# Sem 'Stop' global: o cargo escreve o progresso em stderr, e no PowerShell 5.1 isso
# vira erro terminante quando a saída é redirecionada. Os passos checam $LASTEXITCODE.
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
}

$iscc = @(
    (Get-Command iscc -ErrorAction SilentlyContinue).Source
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe"
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
) | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
if (-not $iscc) {
    throw 'Inno Setup 6 não encontrado. Instale com: winget install --id JRSoftware.InnoSetup -e'
}

Push-Location $PSScriptRoot
try {
    $version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value

    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build falhou ($LASTEXITCODE)" }

    & $iscc /Q "/DAppVersion=$version" installer\process-monitor.iss
    if ($LASTEXITCODE -ne 0) { throw "Inno Setup falhou ($LASTEXITCODE)" }

    $setup = Join-Path $PSScriptRoot "dist\ProcessMonitor-Setup-$version.exe"
    Write-Host "Instalador: $setup ($([math]::Round((Get-Item $setup).Length / 1MB, 1)) MB)"
} finally {
    Pop-Location
}
