# claunch - instalador para Windows
#
#   irm https://raw.githubusercontent.com/Musul/Claunch-ClaudeCLI-Launcher/main/install.ps1 | iex
#
# Que hace:
#   1. Revisa Git for Windows, Windows Terminal y Claude Code (ofrece instalar lo que falte).
#   2. Descarga claunch.exe de la ultima version publicada y verifica su SHA-256.
#      Si no hay version publicada (o el repo es privado), usa `gh` o compila con cargo.
#   3. Lo instala en %USERPROFILE%\.local\bin, crea los perfiles de Windows Terminal
#      y anade esa carpeta al PATH del usuario.
# Volver a ejecutarlo actualiza a la ultima version.
#
# Opciones (variables de entorno; `irm | iex` no admite parametros):
#   $env:CLAUNCH_VERSION = 'v0.2.0'   version concreta (por defecto, la ultima)
#   $env:CLAUNCH_FROM_SOURCE = '1'    compilar con cargo en vez de descargar
#   $env:CLAUNCH_YES = '1'            aceptar todo sin preguntar (instalaciones desatendidas)
#   $env:CLAUNCH_REPO = 'owner/repo'  instalar desde un fork
#   $env:CLAUNCH_DRY_RUN = '1'        probar sin instalar nada (ni PATH ni perfiles)
#   $env:CLAUNCH_DOWNLOAD_URL = 'https://...'  carpeta con el zip y su .sha256 (espejo o pruebas)
#
# El archivo es ASCII a proposito: PowerShell 5.1 lee mal UTF-8 sin BOM.

& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $Repo = if ($env:CLAUNCH_REPO) { $env:CLAUNCH_REPO } else { 'Musul/Claunch-ClaudeCLI-Launcher' }
    $Version = $env:CLAUNCH_VERSION
    $Asset = 'claunch-windows-x64.zip'
    $Bin = Join-Path $HOME '.local\bin'

    function Step($m) { Write-Host ''; Write-Host "==> $m" -ForegroundColor Cyan }
    function Ok($m) { Write-Host "  [ok] $m" -ForegroundColor Green }
    function Info($m) { Write-Host "       $m" -ForegroundColor DarkGray }
    function Warn($m) { Write-Host "  [!]  $m" -ForegroundColor Yellow }
    function Fail($m) { Write-Host "  [x]  $m" -ForegroundColor Red; throw "claunch: $m" }

    $DryRun = $env:CLAUNCH_DRY_RUN -eq '1'

    function Ask($question) {
        if ($DryRun) { Info "(prueba) $question -> no"; return $false }
        if ($env:CLAUNCH_YES -eq '1') { return $true }
        try { $a = Read-Host "       $question [S/n]" } catch { return $false }
        return ($a -eq '' -or $a -match '^(s|si|y|yes)$')
    }

    function Has-Command($name) { return [bool](Get-Command $name -ErrorAction SilentlyContinue) }

    # Ejecuta un programa externo, muestra su salida (decodificada como UTF-8) y
    # devuelve su codigo de salida. En PowerShell 5.1, con ErrorActionPreference=Stop,
    # cualquier texto en stderr (progreso de cargo, avisos de gh) seria un error fatal.
    function Run($exe, [string[]]$argv, [switch]$Quiet) {
        $prevPref = $ErrorActionPreference
        $prevEnc = [Console]::OutputEncoding
        $ErrorActionPreference = 'Continue'
        [Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false
        try {
            if ($Quiet) {
                & $exe @argv 2>&1 | Out-Null
            } else {
                & $exe @argv 2>&1 | ForEach-Object { Write-Host "       $_" }
            }
            return $LASTEXITCODE
        } finally {
            [Console]::OutputEncoding = $prevEnc
            $ErrorActionPreference = $prevPref
        }
    }

    function Refresh-Path {
        $machine = [Environment]::GetEnvironmentVariable('Path', 'Machine')
        $user = [Environment]::GetEnvironmentVariable('Path', 'User')
        $env:Path = (@($machine, $user, $Bin) | Where-Object { $_ }) -join ';'
    }

    function Find-GitBash {
        $candidates = @(
            "$env:ProgramFiles\Git\bin\bash.exe",
            "${env:ProgramFiles(x86)}\Git\bin\bash.exe",
            "$env:LOCALAPPDATA\Programs\Git\bin\bash.exe"
        )
        $git = Get-Command git.exe -ErrorAction SilentlyContinue
        if ($git) { $candidates = @((Join-Path (Split-Path (Split-Path $git.Source)) 'bin\bash.exe')) + $candidates }
        return $candidates | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
    }

    function Winget-Install($id, $label) {
        if (-not (Has-Command winget)) {
            Warn "No tengo winget para instalar $label. Instalalo a mano y vuelve a ejecutar este comando."
            return $false
        }
        if (-not (Ask "Falta $label. Lo instalo con winget?")) { return $false }
        [void](Run 'winget' @('install', '--id', $id, '-e', '--source', 'winget', '--accept-package-agreements', '--accept-source-agreements'))
        Refresh-Path
        return $true
    }

    Write-Host ''
    Write-Host '  claunch - launcher de sesiones de Claude Code' -ForegroundColor DarkYellow
    Write-Host "  https://github.com/$Repo" -ForegroundColor DarkGray

    if ($env:OS -ne 'Windows_NT') { Fail 'Este instalador es para Windows. En Linux usa install.sh (ver README).' }
    if (-not [Environment]::Is64BitOperatingSystem) { Fail 'claunch necesita Windows de 64 bits.' }

    # ------------------------------------------------------------ requisitos
    Step 'Revisando requisitos'
    if (-not (Find-GitBash)) {
        [void](Winget-Install 'Git.Git' 'Git for Windows (Git Bash)')
        if (-not (Find-GitBash)) { Fail 'claunch necesita Git for Windows: https://git-scm.com/download/win' }
    }
    Ok "Git Bash: $(Find-GitBash)"

    $wtAlias = Join-Path $env:LOCALAPPDATA 'Microsoft\WindowsApps\wt.exe'
    if ((Has-Command wt.exe) -or (Test-Path $wtAlias)) {
        Ok 'Windows Terminal'
    } else {
        [void](Winget-Install 'Microsoft.WindowsTerminal' 'Windows Terminal')
        if ((Has-Command wt.exe) -or (Test-Path $wtAlias)) { Ok 'Windows Terminal' } else { Warn 'Sin Windows Terminal: claunch solo podra abrir sesiones en la terminal actual.' }
    }

    if ((Has-Command claude) -or (Test-Path (Join-Path $Bin 'claude.exe'))) {
        Ok 'Claude Code'
    } else {
        Warn 'No encuentro Claude Code (comando claude).'
        if (Ask 'Lo instalo con el instalador oficial (https://claude.ai/install.ps1)?') {
            Invoke-RestMethod https://claude.ai/install.ps1 | Invoke-Expression
            Refresh-Path
        } else {
            Info 'Instalalo despues con:  irm https://claude.ai/install.ps1 | iex'
        }
    }

    # ------------------------------------------------------------ binario
    $tmp = Join-Path ([IO.Path]::GetTempPath()) ("claunch-install-" + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        $exe = $null
        $src = $null
        if ($PSCommandPath) { $src = Split-Path -Parent $PSCommandPath }

        if ($env:CLAUNCH_FROM_SOURCE -ne '1') {
            Step 'Descargando claunch'
            $base = if ($env:CLAUNCH_DOWNLOAD_URL) { $env:CLAUNCH_DOWNLOAD_URL.TrimEnd('/') } elseif ($Version) { "https://github.com/$Repo/releases/download/$Version" } else { "https://github.com/$Repo/releases/latest/download" }
            $zip = Join-Path $tmp $Asset
            $sum = "$zip.sha256"
            $downloaded = $false
            try {
                Invoke-WebRequest -UseBasicParsing -Uri "$base/$Asset" -OutFile $zip
                Invoke-WebRequest -UseBasicParsing -Uri "$base/$Asset.sha256" -OutFile $sum
                $downloaded = $true
            } catch {
                Info "Descarga publica no disponible ($($_.Exception.Message))."
                if (Has-Command gh) {
                    Info 'Pruebo con GitHub CLI (sirve para repos privados)...'
                    $ghArgs = @('release', 'download')
                    if ($Version) { $ghArgs += $Version }
                    $ghArgs += @('--repo', $Repo, '--pattern', $Asset, '--pattern', "$Asset.sha256", '--dir', $tmp, '--clobber')
                    $code = Run 'gh' $ghArgs -Quiet
                    $downloaded = ($code -eq 0) -and (Test-Path $zip) -and (Test-Path $sum)
                }
            }
            if ($downloaded) {
                $expected = ((Get-Content $sum -Raw).Trim() -split '\s+')[0].ToLower()
                $actual = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
                if ($expected -ne $actual) { Fail "SHA-256 no coincide (esperado $expected, obtenido $actual). No instalo nada." }
                Ok 'SHA-256 verificado'
                Expand-Archive -Path $zip -DestinationPath $tmp -Force
                $exe = Join-Path $tmp 'claunch.exe'
            } else {
                Warn 'No hay binario publicado al que pueda acceder.'
            }
        }

        if (-not $exe -or -not (Test-Path $exe)) {
            Step 'Compilando desde el codigo fuente'
            if (-not (Has-Command cargo)) {
                Fail 'Necesito Rust para compilar (https://rustup.rs, con Visual Studio Build Tools C++). O publica una version: ver README, seccion Publicar.'
            }
            if ($src -and (Test-Path (Join-Path $src 'Cargo.toml'))) {
                Info "Usando el codigo en $src"
                $code = Run 'cargo' @('build', '--release', '--locked', '--manifest-path', (Join-Path $src 'Cargo.toml'))
                if ($code -ne 0) { Fail 'cargo build fallo.' }
                $exe = Join-Path $src 'target\release\claunch.exe'
            } else {
                $gitArgs = @('install', '--git', "https://github.com/$Repo", '--locked', '--root', $tmp)
                if ($Version) { $gitArgs += @('--tag', $Version) }
                $code = Run 'cargo' $gitArgs
                if ($code -ne 0) { Fail 'cargo install fallo. Si el repo es privado, ejecuta antes:  gh auth login  y  gh auth setup-git' }
                $exe = Join-Path $tmp 'bin\claunch.exe'
            }
        }
        if (-not (Test-Path $exe)) { Fail "No se genero $exe" }

        # -------------------------------------------------------- instalar
        if ($DryRun) {
            Step 'Prueba: no instalo nada'
            $ver = & $exe --version
            Ok "$ver listo en $exe"
            Info "Instalaria en $Bin, anadiria esa carpeta al PATH y ejecutaria: claunch setup"
            return
        }
        Step 'Instalando'
        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        $parts = @($userPath -split ';' | Where-Object { $_ })
        $inPath = $parts | Where-Object { $_.TrimEnd('\') -ieq $Bin.TrimEnd('\') }
        if (-not $inPath) {
            [Environment]::SetEnvironmentVariable('Path', (@($parts) + $Bin) -join ';', 'User')
            Ok "Anadido al PATH del usuario: $Bin"
        }
        Refresh-Path

        $code = Run $exe @('setup')
        if ($code -ne 0) { Fail 'claunch setup fallo (mira el mensaje de arriba).' }

        $installed = Join-Path $Bin 'claunch.exe'
        $ver = & $installed --version
        Ok "$ver en $installed"
    } finally {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    }

    Write-Host ''
    Write-Host '  Listo. Abre una terminal nueva y escribe:  claunch' -ForegroundColor Green
    Write-Host "  En Windows Terminal tambien tienes el perfil 'claunch'." -ForegroundColor DarkGray
    Write-Host '  Para actualizar, vuelve a ejecutar el mismo comando.' -ForegroundColor DarkGray
    Write-Host ''
}
