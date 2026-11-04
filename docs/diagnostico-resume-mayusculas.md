# Abrir en la terminal las sesiones de Claude Code creadas en VS Code

Diagnóstico del 2026-11-04 que dio origen a claunch.

## Problema

`/resume` en la terminal no mostraba las sesiones de un repo creadas desde la
extensión de VS Code, aunque existían.

## Causa (confirmada)

| Origen | `cwd` que guarda la sesión |
|---|---|
| Extensión de VS Code (`entrypoint: claude-vscode`) | `c:\Users\...` (c minúscula) |
| CLI lanzada desde Git Bash o PowerShell | `C:\Users\...` (C mayúscula) |

- En disco van a la misma carpeta, `~\.claude\projects\c--Users-...-<repo>\`,
  porque Windows no distingue mayúsculas.
- `/resume` (y `-c`) comparan el `cwd` como texto: desde la terminal no ven las
  de VS Code, y al revés.
- Descartado: limpieza de transcripts (`cleanupPeriodDays` en
  `~\.claude\settings.json`) y worktrees.
- Confirmado: arrancando con el `cwd` en minúscula aparecieron las sesiones de
  VS Code.

## Primera solución: función `cc` en `~/.bashrc` (Git Bash)

Arranca `claude` a través de `cmd //c "cd /d c:\..."`, así hereda el `cwd` con
la unidad en minúscula, igual que VS Code. Siempre abre en la raíz del repo
(`git rev-parse --show-toplevel`), para que una subcarpeta no cree otro
historial. Está en [`legacy/cc.sh`](../legacy/cc.sh); claunch aplica la misma
regla (ver `src/paths.rs`).

```bash
cc                 # sesión nueva en la raíz del repo donde estés
cc -c              # continúa la última sesión de ese repo
cc -r              # selector de sesiones de ese repo (incluye las de VS Code)
cc tienda -c       # salta al repo de ~/Desktop/Github que contenga "tienda"
cc tien<Tab>       # autocompleta el nombre del repo
cc -r <id>         # abre una sesión concreta por id
```

- Nombre ambiguo (`cc web`): lista las coincidencias y no abre nada.
  Si uno coincide exacto (`cc api`), abre ese.
- Los argumentos con espacios, comillas o `&` llegan intactos (se pasan por la
  variable `CC_CMD` para que Git Bash no los re-escape).
- Carpeta de repos: `CC_REPOS` (por defecto `~/Desktop/Github`).
- `CC_BIN` cambia el ejecutable (solo para pruebas).

Para instalarla: `cat legacy/cc.sh >> ~/.bashrc && source ~/.bashrc`. Para
quitarla, borra de `~/.bashrc` el bloque entre `# --- Claude Code: cc` y la
línea de guiones final.

## Cómo se diagnosticó

1. Carpetas de `~\.claude\projects\` del repo y número de `.jsonl`.
2. Dentro de cada `.jsonl`: campos `"cwd"` y `"entrypoint"`. Todas las de VS
   Code tenían `c:\`, la de la CLI `C:\`.
3. Comprobado que Git Bash entrega `C:\` a un programa nativo y que
   `cmd //c "cd /d c:\..."` conserva la minúscula.

## Pruebas hechas

Con `legacy/prueba-fake.js` como ejecutable falso (imprime cwd y argumentos):

```bash
source legacy/cc.sh
CC_BIN="node $(cygpath -w "$PWD/legacy/prueba-fake.js")" cc tienda -c
```

| Caso | Resultado |
|---|---|
| Desde una subcarpeta, sin argumentos | `cwd=c:\...\<repo>` (raíz, minúscula) |
| `cc -c` | args `["-c"]` |
| `cc docs -r 'hola "mundo" & cosas'` | repo correcto, texto intacto |
| `cc api` | abre `api` exacto |
| `cc web` | lista las coincidencias, no abre |
| `cc noexiste` | error claro |
| `cc tienda --version` | versión real de Claude Code |
| `cc tienda -r` | aparecen las sesiones de VS Code |

## Riesgos

- `cc` existe solo en Git Bash, no en PowerShell ni cmd.
- Retomar en la terminal una sesión abierta a la vez en VS Code: dos procesos
  escriben el mismo `.jsonl`. Cierra la de VS Code antes.
- `cc` es también el nombre clásico del compilador de C; si instalas uno,
  renombra la función.
