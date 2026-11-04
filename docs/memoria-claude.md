---
name: resume-filtra-por-mayuscula-de-unidad
description: "/resume en la CLI oculta las sesiones de VS Code por la letra de unidad (c: vs C:); el usuario abre con la función cc de ~/.bashrc"
metadata:
  node_type: memory
  type: reference
  modified: 2026-11-04T14:38:06.207Z
---

La extensión de VS Code guarda las sesiones con `cwd` `c:\...` (minúscula); la CLI lanzada desde Git Bash o PowerShell usa `C:\...`. Las dos van a la misma carpeta de `~/.claude/projects/` (Windows no distingue mayúsculas), pero `/resume` y `-c` comparan el `cwd` como texto y ocultan las de la otra superficie. Confirmado por el usuario el 2026-11-04.

Solución instalada el 2026-11-04 en `~/.bashrc` (Git Bash): función `cc` que lanza `claude` vía `cmd //c` con la unidad en minúscula y en la raíz del repo (`cc`, `cc -c`, `cc -r`, `cc <parte-del-nombre> [flags]`, Tab completa repos de `~/Desktop/Github`).

**How to apply:** si "no aparecen mis sesiones", mirar el `cwd` y `entrypoint` dentro de los `.jsonl` antes de suponer que se perdieron. Una sesión concreta se abre siempre con `claude -r <id>`.
