<#
.SYNOPSIS
    Compila e abre o Process Monitor a partir deste terminal.

.DESCRIPTION
    Em debug o executável mantém o console, então panics e logs aparecem aqui.
    -Release abre o build otimizado, sem console (o mesmo que vai no instalador).
#>
[CmdletBinding()]
param(
    [switch]$Release
)

# Sem $ErrorActionPreference = 'Stop': o cargo escreve o progresso em stderr, e no
# PowerShell 5.1 isso vira erro terminante quando a saída é redirecionada.

# O rustup instala em ~/.cargo/bin, que só entra no PATH de terminais abertos depois.
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
}

Push-Location $PSScriptRoot
try {
    $cargoArgs = @('run')
    if ($Release) { $cargoArgs += '--release' }
    cargo @cargoArgs
} finally {
    Pop-Location
}
