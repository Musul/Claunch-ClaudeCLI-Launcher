# --- Claude Code: cc -----------------------------------------------------
#   cc               sesión nueva en la raíz del repo donde estás
#   cc -c            continúa la última sesión de este repo
#   cc -r            selector de sesiones de este repo
#   cc tienda        salta al repo de ~/Desktop/Github que contenga "tienda"
#   cc tienda -c     ...y continúa su última sesión (Tab autocompleta el nombre)
# Arranca con la unidad en minúscula (c:\...), igual que la extensión de
# VS Code, para que /resume vea las sesiones creadas en las dos.
CC_REPOS="${CC_REPOS:-$HOME/Desktop/Github}"
cc() {
  local dir matches exact
  if [ $# -gt 0 ] && [ "${1#-}" = "$1" ]; then
    matches=$(find "$CC_REPOS" -mindepth 1 -maxdepth 1 -type d -iname "*$1*" | sort)
    exact=$(printf '%s\n' "$matches" | grep -i "/$1\$")
    [ -n "$exact" ] && matches=$exact
    if [ -z "$matches" ]; then
      echo "cc: ningún repo en $CC_REPOS contiene '$1'"; return 1
    elif [ "$(printf '%s\n' "$matches" | wc -l)" -gt 1 ]; then
      echo "cc: '$1' coincide con varios:"; printf '%s\n' "$matches" | sed 's#.*/#  #'; return 1
    fi
    dir=$matches; shift
  else
    dir=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
  fi
  local win args="" a
  win=$(cygpath -w "$dir")
  win="$(printf '%s' "${win:0:1}" | tr 'A-Z' 'a-z')${win:1}"
  for a in "$@"; do
    case "$a" in *[[:space:]\"\&\|\<\>^]*) a="\"${a//\"/\\\"}\"";; esac
    args="$args $a"
  done
  echo "cc: $win"
  CC_CMD="cd /d \"$win\" && ${CC_BIN:-claude}$args" cmd //c %CC_CMD%
}
_cc_complete() {
  [ "$COMP_CWORD" -eq 1 ] || return
  COMPREPLY=($(cd "$CC_REPOS" 2>/dev/null && compgen -d -- "${COMP_WORDS[1]}"))
}
complete -F _cc_complete cc
# -------------------------------------------------------------------------
