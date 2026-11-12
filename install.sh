#!/bin/sh
# claunch - instalador para Linux
#
#   curl -fsSL https://raw.githubusercontent.com/Musul/Claunch-ClaudeCLI-Launcher/main/install.sh | sh
#
# Qué hace:
#   1. Revisa Claude Code (ofrece el instalador oficial si falta) y dónde podrá
#      abrir sesiones: tmux o un escritorio gráfico.
#   2. Descarga claunch de la última versión publicada y verifica su SHA-256.
#      Si no hay binario para tu arquitectura o no se puede descargar, usa `gh`
#      o compila con cargo.
#   3. Lo instala en ~/.local/bin (claunch setup) y, si esa carpeta no está en
#      el PATH, ofrece añadirla a ~/.bashrc o ~/.zshrc.
# Volver a ejecutarlo actualiza a la última versión.
#
# Opciones (variables de entorno, p. ej. `curl ... | CLAUNCH_YES=1 sh`):
#   CLAUNCH_VERSION=v0.3.0     versión concreta (por defecto, la última)
#   CLAUNCH_FROM_SOURCE=1      compilar con cargo en vez de descargar
#   CLAUNCH_YES=1              aceptar todo sin preguntar (instalaciones desatendidas)
#   CLAUNCH_REPO=owner/repo    instalar desde un fork
#   CLAUNCH_DRY_RUN=1          probar sin instalar nada
#   CLAUNCH_DOWNLOAD_URL=...   carpeta con el .tar.gz y su .sha256 (espejo o pruebas)

set -eu

# Todo va dentro de main: si la descarga del script se corta, no se ejecuta a medias.
main() {
    REPO=${CLAUNCH_REPO:-Musul/Claunch-ClaudeCLI-Launcher}
    VERSION=${CLAUNCH_VERSION:-}
    DRY_RUN=${CLAUNCH_DRY_RUN:-0}
    BIN="$HOME/.local/bin"

    if [ -t 1 ]; then
        C_STEP='\033[36m' C_OK='\033[32m' C_DIM='\033[2m' C_WARN='\033[33m' C_ERR='\033[31m' C_ACC='\033[38;2;217;119;87m' C_END='\033[0m'
    else
        C_STEP='' C_OK='' C_DIM='' C_WARN='' C_ERR='' C_ACC='' C_END=''
    fi

    printf '\n  %bclaunch - launcher de sesiones de Claude Code%b\n' "$C_ACC" "$C_END"
    printf '  %bhttps://github.com/%s%b\n' "$C_DIM" "$REPO" "$C_END"

    [ "$(uname -s)" = Linux ] || fail "Este instalador es para Linux. En Windows usa install.ps1 (ver README)."

    case $(uname -m) in
        x86_64 | amd64) ASSET=claunch-linux-x64.tar.gz ;;
        *) ASSET='' ;;
    esac

    if has curl; then
        DL=curl
    elif has wget; then
        DL=wget
    else
        fail "Necesito curl o wget para descargar."
    fi

    # -------------------------------------------------------------- requisitos
    step 'Revisando requisitos'
    if has claude || [ -x "$BIN/claude" ]; then
        ok 'Claude Code'
    else
        warn 'No encuentro Claude Code (comando claude).'
        if ask '¿Lo instalo con el instalador oficial (https://claude.ai/install.sh)?'; then
            fetch https://claude.ai/install.sh - | bash
        else
            info 'Instálalo después con:  curl -fsSL https://claude.ai/install.sh | bash'
        fi
    fi

    if [ -n "${TMUX:-}" ]; then
        ok 'tmux (claunch está dentro de tmux: pestañas y paneles)'
    elif has tmux; then
        ok 'tmux (ejecuta claunch dentro de tmux para pestañas y paneles)'
    fi
    if [ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]; then
        ok 'escritorio gráfico (una ventana por sesión fuera de tmux)'
    elif ! has tmux; then
        warn 'Sin tmux ni escritorio gráfico: solo podrás abrir sesiones en la terminal actual.'
        info 'Instala tmux para abrir varias sesiones a la vez (p. ej. sudo apt install tmux).'
    fi

    # -------------------------------------------------------------- binario
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT INT TERM
    exe=''

    src=''
    case $0 in
        */install.sh | install.sh) src=$(cd "$(dirname "$0")" && pwd) ;;
    esac

    if [ "${CLAUNCH_FROM_SOURCE:-0}" != 1 ]; then
        if [ -z "$ASSET" ]; then
            info "No hay binario publicado para $(uname -m): compilo desde el código."
        else
            step 'Descargando claunch'
            if [ -n "${CLAUNCH_DOWNLOAD_URL:-}" ]; then
                base=${CLAUNCH_DOWNLOAD_URL%/}
            elif [ -n "$VERSION" ]; then
                base="https://github.com/$REPO/releases/download/$VERSION"
            else
                base="https://github.com/$REPO/releases/latest/download"
            fi
            downloaded=0
            if fetch "$base/$ASSET" "$tmp/$ASSET" && fetch "$base/$ASSET.sha256" "$tmp/$ASSET.sha256"; then
                downloaded=1
            else
                info 'Descarga pública no disponible.'
                if has gh; then
                    info 'Pruebo con GitHub CLI (sirve para repos privados)...'
                    # shellcheck disable=SC2086 # $VERSION vacío = última versión
                    if gh release download $VERSION --repo "$REPO" --pattern "$ASSET" --pattern "$ASSET.sha256" \
                        --dir "$tmp" --clobber >/dev/null 2>&1; then
                        downloaded=1
                    fi
                fi
            fi
            if [ "$downloaded" = 1 ]; then
                expected=$(cut -d ' ' -f 1 <"$tmp/$ASSET.sha256" | tr 'A-F' 'a-f')
                actual=$(sha256 "$tmp/$ASSET")
                [ "$expected" = "$actual" ] || fail "SHA-256 no coincide (esperado $expected, obtenido $actual). No instalo nada."
                ok 'SHA-256 verificado'
                tar -xzf "$tmp/$ASSET" -C "$tmp"
                exe="$tmp/claunch"
            else
                warn 'No hay binario publicado al que pueda acceder.'
            fi
        fi
    fi

    if [ -z "$exe" ]; then
        step 'Compilando desde el código fuente'
        has cargo || fail 'Necesito Rust para compilar (https://rustup.rs). O publica una versión: ver README, sección Publicar.'
        if [ -n "$src" ] && [ -f "$src/Cargo.toml" ]; then
            info "Usando el código en $src"
            cargo build --release --locked --manifest-path "$src/Cargo.toml" || fail 'cargo build falló.'
            exe="${CARGO_TARGET_DIR:-$src/target}/release/claunch"
        else
            if [ -n "$VERSION" ]; then
                cargo install --git "https://github.com/$REPO" --locked --root "$tmp" --tag "$VERSION" ||
                    fail 'cargo install falló. Si el repo es privado, ejecuta antes:  gh auth login  y  gh auth setup-git'
            else
                cargo install --git "https://github.com/$REPO" --locked --root "$tmp" ||
                    fail 'cargo install falló. Si el repo es privado, ejecuta antes:  gh auth login  y  gh auth setup-git'
            fi
            exe="$tmp/bin/claunch"
        fi
    fi
    [ -x "$exe" ] || fail "No se generó $exe"

    # -------------------------------------------------------------- instalar
    if [ "$DRY_RUN" = 1 ]; then
        step 'Prueba: no instalo nada'
        ok "$("$exe" --version) listo en $exe"
        info "Instalaría en $BIN con: claunch setup"
        return 0
    fi
    step 'Instalando'
    "$exe" setup >/dev/null || fail 'claunch setup falló (ejecuta el binario con «setup» para ver el motivo).'
    ok "$("$BIN/claunch" --version) en $BIN/claunch"
    ensure_path

    printf '\n  %bListo. Escribe:  claunch%b\n' "$C_OK" "$C_END"
    printf '  %bPara actualizar, vuelve a ejecutar el mismo comando.%b\n\n' "$C_DIM" "$C_END"
}

step() { printf '\n%b==> %s%b\n' "$C_STEP" "$1" "$C_END"; }
ok() { printf '  %b[ok]%b %s\n' "$C_OK" "$C_END" "$1"; }
info() { printf '  %b     %s%b\n' "$C_DIM" "$1" "$C_END"; }
warn() { printf '  %b[!]%b  %s\n' "$C_WARN" "$C_END" "$1"; }
fail() {
    printf '  %b[x]%b  %s\n' "$C_ERR" "$C_END" "$1" >&2
    exit 1
}

has() { command -v "$1" >/dev/null 2>&1; }

# fetch URL DESTINO ("-" = salida estándar)
fetch() {
    if [ "$DL" = curl ]; then
        curl -fsSL --retry 2 -o "$2" "$1"
    else
        wget -q -O "$2" "$1"
    fi
}

sha256() {
    if has sha256sum; then
        sha256sum "$1" | cut -d ' ' -f 1
    else
        shasum -a 256 "$1" | cut -d ' ' -f 1
    fi
}

# Pregunta S/n en la terminal (también con `curl | sh`, que ocupa la entrada estándar).
ask() {
    if [ "$DRY_RUN" = 1 ]; then
        info "(prueba) $1 -> no"
        return 1
    fi
    [ "${CLAUNCH_YES:-0}" = 1 ] && return 0
    if ! (: </dev/tty) 2>/dev/null; then
        info "$1 -> no (sin terminal para preguntar; usa CLAUNCH_YES=1)"
        return 1
    fi
    printf '       %s [S/n] ' "$1" >/dev/tty
    read -r answer </dev/tty || return 1
    case $answer in
        '' | s | S | si | Si | SI | sí | Sí | y | Y | yes) return 0 ;;
        *) return 1 ;;
    esac
}

# Si ~/.local/bin no está en el PATH, ofrece añadirlo al archivo de la shell.
ensure_path() {
    case ":$PATH:" in
        *":$BIN:"*) return 0 ;;
    esac
    # shellcheck disable=SC2016 # se escribe literal: lo expande la shell al abrirse
    line='export PATH="$HOME/.local/bin:$PATH"'
    case ${SHELL:-} in
        bash | */bash) rc="$HOME/.bashrc" ;;
        zsh | */zsh) rc="${ZDOTDIR:-$HOME}/.zshrc" ;;
        *) rc='' ;;
    esac
    if [ -n "$rc" ] && grep -qs '\.local/bin' "$rc"; then
        info "$rc ya añade ~/.local/bin al PATH: abre una terminal nueva."
    elif [ -n "$rc" ] && ask "$BIN no está en el PATH. ¿Lo añado a $rc?"; then
        printf '\n# claunch\n%s\n' "$line" >>"$rc"
        ok "Añadido a $rc (abre una terminal nueva)"
    else
        warn "$BIN no está en el PATH. Añade a tu shell:  $line"
    fi
}

main "$@"
