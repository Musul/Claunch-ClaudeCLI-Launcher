# Agentes compatibles

claunch indexa y lanza sesiones de cinco agentes. Cada uno guarda su historial a
su manera; aquí está qué lee claunch de cada uno, con qué argumentos lo lanza y
qué no se puede hacer. Todo se lee **en solo lectura**: claunch no modifica los
datos de ningún agente ni los ejecuta para listar sesiones. Las bases SQLite se
abren en solo lectura y sin hacer checkpoint; al leerlas, SQLite puede dejar
junto a la base sus archivos auxiliares (`-shm`, `-wal`).

`claunch agents` muestra qué agentes ve claunch en tu equipo, con qué ejecutable
los lanza, dónde busca sus datos y cuántas sesiones encontró (`--json` para
scripts; `--installed` da solo los nombres de los instalados, sin indexar nada).

## Qué es igual para todos

- **Detección**: un agente está disponible si su ejecutable está en el `PATH` (o
  en una de sus ubicaciones habituales). Sus sesiones se listan aunque el
  ejecutable falte; al abrir una, claunch dice cómo instalarlo.
- **Configuración** (`config.toml`): `[harness.<id>]` con `enabled = false` (ni
  se indexa ni se ofrece), `command = ["ruta", "arg"]` (otro ejecutable o
  argumentos previos) y `home = "carpeta"` (otra carpeta de datos). Los `id` son
  `claude`, `codex`, `opencode`, `antigravity` y `pi`.
- **Sesión nueva**: en la pantalla de un repo, `⇥` elige el agente y `⏎` la abre.
  `general.default_harness` fija el agente inicial.
- **Carpeta**: Claude arranca con la unidad en minúscula (`c:\…`, ver
  [diagnostico-resume-mayusculas.md](diagnostico-resume-mayusculas.md)); los
  demás, con la ruta como la da Windows (`C:\…`).
- **Sesiones en vivo**: Claude publica el estado de cada proceso y claunch lo
  lee. Los demás no: claunch marca como abiertas las sesiones que abrió él
  (mientras viva el proceso) y deduce «trabajando» de lo último que el agente
  guardó. No distingue «espera un permiso» ni ve sesiones abiertas por otra vía.
  Si un agente muere a mitad de un turno, su sesión no queda como abierta (el
  proceso ya no existe), aunque su historial diga que trabajaba; al reanudarla
  no sale «trabajando» hasta que el proceso nuevo guarda algo.
- **Sesiones nuevas sin id** (Codex, opencode y Antigravity no dejan fijarlo):
  la pestaña sale como abierta desde el principio y claunch la une a su sesión
  cuando esta aparece, normalmente con el primer prompt. Solo lo hace si no hay
  duda. Con varias pestañas nuevas del mismo agente en la misma carpeta que aún
  no tienen sesión, la sesión es de la única que acababa de abrirse cuando
  nació: 30 s en Codex, que fecha la sesión al arrancar, y 10 min en opencode y
  Antigravity, que la crean con el primer prompt. Si no hay una sola (se
  abrieron a la vez, o llevan todas un rato sin usar), no se sabe de cuál es y
  esas pestañas siguen como abiertas sin más detalle. Por lo mismo, si abres
  una pestaña de opencode o Antigravity y, sin usarla, escribes antes de 10 min
  en otra más antigua que seguía sin estrenar, la sesión se atribuye a la
  nueva. Si dentro del agente empiezas otra sesión (`/new`), claunch sigue
  mostrando la primera. Una sesión creada en esa carpeta desde otra terminal
  mientras la pestaña sigue sin usar puede tomarse por la suya.
- **Lo que solo tiene Claude**: barra de uso del plan, mapa de mensajes entre
  sesiones, subagentes activos y bypass global de permisos.

## Claude Code (`claude`)

- **Datos**: `~/.claude/projects/*/<uuid>.jsonl` (o `CLAUDE_CONFIG_DIR`). Lista
  las sesiones de la CLI, de VS Code y de la aplicación de escritorio.
- **Lanzamiento**: nueva `claude --session-id <uuid>` (claunch fija el id),
  reanudar `claude -r <id>`, fork `claude -r <id> --fork-session`.
- **Opciones**: modelo (`--model`), permisos (`--permission-mode`), nombre (`-n`)
  y worktree (`-w`).
- **Ejecutable**: `claude` del `PATH`; si no está, `~/.local/bin/claude` (donde
  lo deja su instalador).
- **En vivo**: `~/.claude/sessions/<pid>.json` (trabajando, espera, inactiva),
  también para sesiones abiertas fuera de claunch.

## Codex (`codex`)

- **Datos**: los rollouts `.jsonl` de `~/.codex/sessions` (o `CODEX_HOME`).
  claunch no abre las bases SQLite de Codex ni sondea sus bloqueos.
- **Qué lista**: las sesiones del usuario creadas desde la CLI, VS Code o la
  aplicación, incluidas sus bifurcaciones. Quedan fuera los subagentes, los
  hilos hijos, las sesiones no interactivas (`exec`, `mcp`), las archivadas y
  los rollouts comprimidos (`.zst`). Muestra carpeta, rama, modelo, superficie,
  número de prompts y primer y último prompt (sin el contexto que añade el IDE).
  El título es el nombre del hilo (`session_index.jsonl`) o, si no tiene, el
  primer prompt.
- **Lanzamiento**: nueva `codex`, reanudar `codex resume <id>`, fork
  `codex fork <id>`. Codex no deja fijar el id ni el nombre de una sesión nueva.
- **Opciones**: modelo (`--model`, texto libre) y permisos:

  | Modo | Flags |
  |---|---|
  | preguntar | `-a on-request -s workspace-write` |
  | solo lectura | `-s read-only` |
  | sin preguntar | `-a never -s workspace-write` |
  | sin sandbox | `--dangerously-bypass-approvals-and-sandbox` |

- **Ejecutable**: `codex` del `PATH`; si no está, el más reciente entre los de
  la aplicación de escritorio, la extensión de VS Code, el paquete que gestiona
  Codex en `CODEX_HOME` y `~/.local/bin`. Un ejecutable
  antiguo puede no reanudar sesiones creadas por uno más nuevo: instala la CLI
  (`npm install -g @openai/codex`) o fija la ruta en `[harness.codex] command`.
- **En vivo**: «trabajando» es un turno empezado y sin terminar en el rollout.
  Codex no guarda cuándo espera una aprobación.
- **Sin verificar**: Linux y versiones de Codex posteriores a la 0.145 (los
  datos de prueba llegan hasta ahí).

## opencode 2.x (`opencode`)

- **Datos**: la base SQLite `~/.local/share/opencode/opencode.db` (también en
  Windows; respeta `XDG_DATA_HOME` y `OPENCODE_DB`). claunch la abre en solo
  lectura y con consultas cortas; si está ocupada o ilegible, conserva la lista
  anterior y lo anota una vez en el log.
- **Qué lista**: las sesiones raíz no archivadas y los forks. Quedan fuera los
  subagentes (sesiones hijas) y las archivadas. Muestra título, carpeta, primer y
  último prompt, número de prompts y modelo (`proveedor/modelo#variante`). La
  base no guarda rama git ni desde dónde se creó la sesión.
- **Lanzamiento**: nueva `opencode`, reanudar `opencode --session <id>`.
- **Opciones**: permisos «por defecto» o «auto» (`--auto`: aprueba lo que no esté
  denegado). La interfaz de opencode no admite por línea de comandos fork,
  modelo, nombre ni worktree, así que claunch no los ofrece.
- **Ejecutable**: `opencode` del `PATH`; si no está, `~/.opencode/bin/opencode`
  (donde lo deja su instalador).
- **Versiones**: solo opencode 2.x (tabla `session_v2`). Con una base de otra
  versión no lista sesiones y lo anota en el log.
- **Sin verificar**: Linux (los datos de prueba son de Windows) y el efecto exacto de `--auto`.

## Antigravity (`agy`)

- **Datos**: los índices `conversation_summaries.db` (SQLite, en solo lectura)
  de `~/.gemini/antigravity-cli` (CLI) y `~/.gemini/antigravity` (aplicación e
  IDE). claunch nunca ejecuta `agy` para listar: arrancarlo, incluso con
  `--help`, dispara su actualizador y escribe en su carpeta.
- **Qué lista**: las conversaciones con carpeta de trabajo, del CLI y también de
  la aplicación y el IDE. Quedan fuera las hijas, las de subagentes y las
  archivadas. Muestra título, carpeta, rama, fechas y, si la conversación está
  en formato `.db`, modelo, número de prompts y último prompt. Las `.pb`
  antiguas no son legibles: se listan solo con lo que dice el índice.
- **Lanzamiento**: nueva `agy`, reanudar `agy --conversation <id>`. No hay fork,
  nombre, worktree ni id fijado por línea de comandos.
- **Opciones**: modelo (`--model`, un id de `agy models`) y permisos:
  `accept-edits`, `auto` y `plan` (`--mode`), o `bypass`
  (`--dangerously-skip-permissions`).
- **Ejecutable**: `~/.gemini/bin/agy` (el que mantiene al día su actualizador);
  si no existe, el del `PATH` o `~/.local/bin/agy`. Ese puede ser un `agy`
  1.0.x, que no tiene `--mode`: al abrir una sesión con uno de esos modos,
  claunch lo comprueba en su `--help` y, si no lo admite, la abre con los
  permisos por defecto y lo avisa en la pestaña.
- **Sin verificar**: que `agy --conversation` continúe una conversación creada
  en la aplicación o el IDE (puede que la clone), el formato de las
  conversaciones creadas por el CLI y todo lo de Linux.

## Pi (`pi`)

- **Datos**: los `.jsonl` de `~/.pi/agent/sessions` (o `PI_CODING_AGENT_DIR`,
  `PI_CODING_AGENT_SESSION_DIR` o el `sessionDir` absoluto de `settings.json`).
- **Qué lista**: las sesiones con al menos un mensaje del usuario. Muestra el
  nombre (`/name` o `--name`; el último gana), carpeta, primer y último prompt,
  número de prompts y modelo (`proveedor/modelo`). Las sesiones con un mensaje de
  contenido nulo no se listan, igual que en Pi.
- **Lanzamiento**: nueva `pi --session-id <uuid>` (claunch fija el id), reanudar
  `pi --session <archivo .jsonl>`, fork `pi --fork <archivo> --session-id <uuid>`.
- **Opciones**: modelo (`--model proveedor/modelo`), nombre (`--name`) y perfil de
  herramientas. Pi no tiene permisos ni sandbox: «solo lectura» deja `read`,
  `grep`, `find` y `ls` (`--tools`) y «sin herramientas» usa `--no-tools`.
- **Límites**: sin worktree ni rama git (Pi no las guarda). El número de prompts
  cuenta todas las ramas de `/tree`. No se ven las sesiones que un proyecto
  guarda en su propia carpeta (`sessionDir` relativo o `.pi/settings.json`).
- **Sin verificar**: Linux y formatos de sesión anteriores a la versión 3 (solo
  con datos de prueba).
