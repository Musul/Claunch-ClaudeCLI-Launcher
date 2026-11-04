# claunch

Launcher TUI para Claude Code en Windows: elige repo → elige sesión (o una
nueva) → se abre en Windows Terminal con Git Bash. Sets para abrir varias
sesiones de golpe en pestañas, paneles divididos o ventanas.

## Uso

```bash
claunch                      # TUI
claunch open "Mi proyecto"   # abre un set sin TUI
claunch bypass status        # estado del bypass global de permisos
claunch bypass on --yes      # activa permissions.defaultMode = bypassPermissions
claunch bypass off
claunch setup                # instala en ~/.local/bin + perfiles de Windows Terminal
claunch setup --uninstall    # borra los perfiles (fragmento)
```

### Pantallas

| Pantalla | Qué muestra |
|---|---|
| Inicio | Repositorios (más reciente primero), Recientes (sesiones de todas las superficies) y Sets |
| Repo | «Nueva sesión» siempre primero y luego sus sesiones, más recientes primero |
| Set | Nombre, diseño (pestañas / paneles / ventanas), ventana (nueva / actual), entradas y vista previa del diseño |

### Teclas

`↑↓`/`jk` mover · `⏎` abrir · `o` abrir con opciones · `⇥`/`←→`/`1 2 3` panel ·
`/` filtrar (⏎ abre) · `n` nuevo set · `e` editar set · `d` borrar ·
`a` añadir sesión al set · `⇧↑↓` reordenar · `B` bypass global · `r` reescanear ·
`?` ayuda · `q` salir. Ratón: click, doble click y rueda.

### Opciones al abrir (`o`)

Destino (pestaña nueva, ventana nueva o esta terminal), shell (Git Bash por
defecto, PowerShell o CMD), modo (reanudar o fork), modelo, permisos y, para
sesiones nuevas, nombre (`-n`) y worktree (`-w`). Muestra el comando `claude`
resultante antes de abrir.

### Guarda contra doble apertura

Antes de reanudar, claunch mira `~/.claude/sessions/<pid>.json` (CLI, VS Code
y Desktop registran ahí cada proceso; se valida que el pid siga vivo y su hora
de creación). Si la sesión ya está abierta, o tuvo actividad hace menos de
90 s, ofrece abrirla como fork, abrir igual o cancelar.

### Confirmación de arranque

Cada pestaña ejecuta `claunch run <spec>`, que deja una marca al arrancar;
la TUI avisa «arrancó» o «sin confirmar tras 15 s».

## Cómo funciona

- **Índice**: lee `~/.claude/projects/*/<uuid>.jsonl` de forma incremental
  (solo los bytes añadidos, validados con una huella del final ya leído) y lo
  guarda en `%LOCALAPPDATA%\claunch\index.json`. Título: nombre puesto por el
  usuario > título IA > primer prompt.
- **Repos**: escanea `general.scan_roots` (por defecto Desktop, Documents,
  Downloads, source, Projects, dev, code, repos; profundidad 5) y une las
  carpetas con sesiones. Subcarpetas y worktrees se agrupan bajo su repo;
  carpetas sin git con sesiones aparecen como «carpeta».
- **Unidad en minúscula**: arranca Claude con `c:\...`, igual que la extensión
  de VS Code, para que `/resume` vea las sesiones de ambas superficies (ver
  [docs/diagnostico-resume-mayusculas.md](docs/diagnostico-resume-mayusculas.md)).
- **Windows Terminal**: `wt -w 0 new-tab` (pestaña), `-w new` (ventana); los
  sets en paneles se reparten en columnas iguales (`pane_grid`).

## Configuración

`%APPDATA%\claunch\config.toml` (la TUI lo reescribe al editar sets):

```toml
[general]
scan_roots = ["~/Desktop", "~/Documents"]
default_shell = "git-bash"      # git-bash | powershell | cmd
default_target = "tab"          # tab | window | here
keep_shell_open = true          # al salir de Claude queda la shell en la carpeta
exit_after_launch = false
wt_profile = ""                 # GUID de perfil WT (setup pone «Claude · Git Bash»)
claude_command = []             # vacío = claude del PATH

[[set]]
name = "Mi proyecto"
layout = "panes"                # tabs | panes | windows
window = "new"                  # new | current

[[set.entry]]
repo = 'c:\Users\...\web-tienda'
session = "latest"              # new | latest | <uuid>
title = "App"
model = "opus"

[[set.entry]]
repo = 'c:\Users\...\api-pagos'
session = "new"
permission_mode = "plan"
```

## Bypass global de permisos

`B` en la TUI o `claunch bypass on --yes` escribe
`permissions.defaultMode = "bypassPermissions"` en `~/.claude/settings.json`:
**todas** las sesiones nuevas ejecutan herramientas sin pedir permiso,
también en repos de producción. Cada cambio respalda el archivo como
`settings.json.bak-claunch-bypass-<AAAAMMDD>`, conserva orden y valores del
resto y verifica que solo cambió esa clave. Al desactivar restaura el modo
anterior. Para una sola sesión es más seguro `o` → Permisos.

## Compilar

Requiere Rust (toolchain MSVC) y Visual Studio Build Tools con la carga C++.

```bash
cargo test
cargo build --release
./target/release/claunch.exe setup
```

Para revisar el diseño sin abrir la TUI:
`claunch snapshot <home|recents|sets|repo|options|new|guard|editor|editor-panes|editor-windows|help|bypass> 140x40 salida.html`.

## Archivos

- `src/data/`: índice de sesiones, repos y sesiones vivas.
- `src/launch.rs`: argumentos de Claude, Windows Terminal y `claunch run`.
- `src/claude_settings.rs`: interruptor del bypass global.
- `src/ui/`: TUI (ratatui + tachyonfx).
- `legacy/cc.sh`: la función `cc` de Git Bash anterior (sigue en `~/.bashrc`).
- `docs/`: diagnóstico del problema de `/resume` y memoria asociada.
