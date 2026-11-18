//! Modo demo: datos de ejemplo para capturas y presentaciones.
//!
//! `claunch demo` abre la interfaz con un equipo ficticio (repos, sesiones en
//! vivo, uso del plan y mensajes entre sesiones) y `claunch snapshot … --demo`
//! la renderiza a HTML. No lee el historial real ni consulta la red, y no
//! escribe nada: la configuración queda en solo lectura, los lanzamientos se
//! descartan y el bypass global no se puede tocar.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use super::app::App;
use crate::claude_settings::BypassStatus;
use crate::config::{Config, LaunchOptions, SessionRef, SessionSet, SetEntry, SetLayout, SetWindow};
use crate::data::sessions::PeerLink;
use crate::data::usage::{Limit, Usage};
use crate::data::{Aliases, Catalog, LiveSession, RootKind, Repo, Session, SessionMeta, UsageState};
use crate::harness::Harness;
use crate::launch::{Env, Terminal};
use crate::paths;

const MIN: i64 = 60_000;
const HOUR: i64 = 60 * MIN;

/// Repos ficticios: (nombre, carpeta padre bajo el perfil, actividad git hace).
const REPOS: [(&str, &str, i64); 10] = [
    ("api-pagos", "Github", 0),
    ("web-tienda", "Github", 2 * MIN),
    ("app-movil", "Github", 5 * MIN),
    ("infra-terraform", "Github", 40 * MIN),
    ("bot-soporte", "Github", 3 * HOUR),
    ("docs-producto", "Documents", 26 * HOUR),
    ("datos-analytics", "Github", 50 * HOUR),
    ("design-system", "Github", 4 * 24 * HOUR),
    ("landing-marketing", "Github", 6 * 24 * HOUR),
    ("scripts-internos", "Documents", 9 * 24 * HOUR),
];

struct Demo {
    harness: Harness,
    id: &'static str,
    repo: usize,
    title: &'static str,
    prompt: &'static str,
    branch: &'static str,
    surface: &'static str,
    age: i64,
    prompts: u32,
    pr: Option<u64>,
    /// Modelo que registra el harness (vacío si no lo guarda).
    model: &'static str,
}

/// Sesiones ficticias de los cinco harness (la primera letra del id identifica
/// la sesión en los mensajes; solo las de Claude se escriben entre sí).
const SESSIONS: [Demo; 18] = [
    Demo { harness: Harness::Claude, id: "a1f0c2d4-7b3e-4c1a-9e2f-5d6b7a8c9e01", repo: 0, title: "Migrar cobros a la API nueva", prompt: "pasa los cobros recurrentes al endpoint v2 y avisa a la sesión del checkout", branch: "feat/cobros-v2", surface: "cli", age: 0, prompts: 34, pr: Some(128), model: "" },
    Demo { harness: Harness::Claude, id: "b2e1d3c5-8a4f-4d2b-8f30-6e7c8b9daf02", repo: 1, title: "Rediseño del checkout", prompt: "adapta el checkout a la respuesta nueva de cobros", branch: "main", surface: "cli", age: MIN, prompts: 21, pr: None, model: "" },
    Demo { harness: Harness::Claude, id: "c3d2e4b6-9b5a-4e3c-a041-7f8d9cae0b03", repo: 2, title: "Notificaciones push de pedidos", prompt: "manda una push cuando el cobro quede confirmado", branch: "feat/push", surface: "claude-desktop", age: 2 * MIN, prompts: 17, pr: Some(64), model: "" },
    Demo { harness: Harness::Claude, id: "d4c3f5a7-ac6b-4f4d-b152-8a9eadbf1c04", repo: 0, title: "Reintentos de webhooks", prompt: "los webhooks fallidos deben reintentarse con backoff", branch: "fix/webhooks", surface: "claude-vscode", age: 9 * MIN, prompts: 12, pr: None, model: "" },
    Demo { harness: Harness::Claude, id: "e5b4a6f8-bd7c-4a5e-8263-9bafbec02d05", repo: 3, title: "Revisión de alertas de producción", prompt: "revisa por qué saltó la alerta de latencia anoche", branch: "main", surface: "cli", age: 14 * MIN, prompts: 8, pr: None, model: "" },
    Demo { harness: Harness::Claude, id: "f6a5b7e9-ce8d-4b6f-9374-acb0cfd13e06", repo: 0, title: "Auditoría de seguridad de pagos", prompt: "audita el manejo de tarjetas y secretos", branch: "chore/auditoria", surface: "cli", age: 5 * HOUR, prompts: 26, pr: Some(121), model: "" },
    Demo { harness: Harness::Claude, id: "a7f6c8da-df9e-4c70-a485-bdc1d0e24f07", repo: 1, title: "Accesibilidad del carrito", prompt: "revisa contraste y foco del carrito", branch: "feat/a11y", surface: "claude-vscode", age: 7 * HOUR, prompts: 15, pr: Some(97), model: "" },
    Demo { harness: Harness::Claude, id: "b8a7d9eb-e0af-4d81-b596-ced2e1f35008", repo: 4, title: "Respuestas automáticas de soporte", prompt: "clasifica los tickets por urgencia", branch: "main", surface: "cli", age: 3 * HOUR, prompts: 19, pr: None, model: "" },
    Demo { harness: Harness::Claude, id: "c9b8eafc-f1b0-4e92-86a7-dfe3f2046109", repo: 5, title: "Guía de onboarding para clientes", prompt: "escribe la guía de primeros pasos", branch: "main", surface: "claude-desktop", age: 26 * HOUR, prompts: 11, pr: None, model: "" },
    Demo { harness: Harness::Pi, id: "dac9fb0d-02c1-4fa3-97b8-e0f4031572a0", repo: 6, title: "Panel de ventas semanal", prompt: "arma el panel con ventas por canal", branch: "", surface: "cli", age: 50 * HOUR, prompts: 23, pr: None, model: "" },
    Demo { harness: Harness::Codex, id: "ebdafc1e-13d2-40b4-a8c9-f105142683b1", repo: 2, title: "Modo oscuro de la app", prompt: "agrega modo oscuro respetando el sistema", branch: "feat/dark", surface: "vscode", age: 30 * HOUR, prompts: 9, pr: None, model: "" },
    Demo { harness: Harness::Antigravity, id: "fcebad2f-24e3-41c5-b9da-0216253794c2", repo: 3, title: "Costos de la nube de octubre", prompt: "resume los costos por servicio", branch: "main", surface: "cli", age: 3 * 24 * HOUR, prompts: 6, pr: None, model: "" },
    Demo { harness: Harness::Claude, id: "0dfcbe30-35f4-42d6-8aeb-1327364805d3", repo: 1, title: "Buscador de productos", prompt: "mejora la relevancia del buscador", branch: "feat/busqueda", surface: "cli", age: 4 * 24 * HOUR, prompts: 31, pr: Some(88), model: "" },
    Demo { harness: Harness::Opencode, id: "ses_1e0dcf414605a3e79bfc243847", repo: 4, title: "Plantillas de respuesta", prompt: "crea plantillas para devoluciones", branch: "", surface: "", age: 5 * 24 * HOUR, prompts: 7, pr: None, model: "claude-sonnet-5-5" },
    Demo { harness: Harness::Codex, id: "019f8a1c-2f1e-7d3c-8b4a-5e6f7a8b9c14", repo: 0, title: "Tests de integración de cobros", prompt: "cubre con tests de integración el endpoint v2 de cobros", branch: "feat/cobros-v2", surface: "cli", age: 3 * MIN, prompts: 14, pr: None, model: "gpt-5.6" },
    Demo { harness: Harness::Opencode, id: "ses_3a2f1e0dcf41b605c3e7d9bf24", repo: 6, title: "Migración a Postgres 17", prompt: "planifica la migración del almacén a Postgres 17", branch: "", surface: "", age: 20 * MIN, prompts: 9, pr: None, model: "claude-sonnet-5-5" },
    Demo { harness: Harness::Antigravity, id: "4b3a2f1e-0dcf-4146-85c3-e7d9bf243816", repo: 5, title: "Diagramas de arquitectura", prompt: "dibuja el diagrama de despliegue con los servicios nuevos", branch: "main", surface: "cli", age: 45 * MIN, prompts: 6, pr: None, model: "gemini-3.8-flash" },
    Demo { harness: Harness::Pi, id: "5c4b3a2f-1e0d-4f41-9605-c3e7d9bf2417", repo: 9, title: "Limpieza de scripts de despliegue", prompt: "unifica los scripts de despliegue en uno solo", branch: "", surface: "cli", age: 70 * MIN, prompts: 11, pr: None, model: "claude-fable-5-1" },
];

fn pipe(i: usize) -> String {
    format!("uds:demo-pipe-{i}")
}

const QA: &str = "bridge:session_01DEMOQA";
const OPS: &str = "bridge:session_01DEMOOPS";

/// Mensajes entre sesiones: (sesión, sale, otro extremo, nombre del otro, hace).
fn links(now: i64) -> Vec<(usize, PeerLink)> {
    let title = |i: usize| SESSIONS[i].title.to_string();
    let out = |peer: String, age: i64| PeerLink { out: true, peer, name: None, id: None, ts_ms: now - age };
    let inc = |peer: String, name: String, age: i64| PeerLink { out: false, peer, name: Some(name), id: None, ts_ms: now - age };
    let mut v = Vec::new();
    // Cobros ↔ checkout: conversación en curso.
    for age in [25_000, 4 * MIN, 12 * MIN] {
        v.push((0, out(pipe(1), age)));
        v.push((1, inc(pipe(0), title(0), age)));
    }
    for age in [50_000, 9 * MIN] {
        v.push((1, out(pipe(0), age)));
        v.push((0, inc(pipe(1), title(1), age)));
    }
    // App móvil → cobros.
    for age in [70_000, 18 * MIN] {
        v.push((2, out(pipe(0), age)));
        v.push((0, inc(pipe(2), title(2), age)));
    }
    // Checkout → app móvil.
    v.push((1, out(pipe(2), 8 * MIN)));
    v.push((2, inc(pipe(1), title(1), 8 * MIN)));
    // QA en otra máquina ↔ cobros.
    v.push((0, inc(QA.into(), "QA · pruebas de pago".into(), 3 * MIN)));
    v.push((0, out(QA.into(), 150_000)));
    v.push((0, inc(QA.into(), "QA · pruebas de pago".into(), 40 * MIN)));
    // Ops en otra máquina ↔ alertas.
    v.push((4, inc(OPS.into(), "Ops · guardia".into(), 50 * MIN)));
    v.push((4, out(OPS.into(), 45 * MIN)));
    // Auditoría (ya cerrada) → cobros y webhooks.
    v.push((5, out(pipe(0), 5 * HOUR)));
    v.push((0, inc(pipe(5), title(5), 5 * HOUR)));
    v.push((5, out(pipe(3), 5 * HOUR + 10 * MIN)));
    v.push((3, inc(pipe(5), title(5), 5 * HOUR + 10 * MIN)));
    // Soporte → checkout.
    v.push((7, out(pipe(1), 3 * HOUR)));
    v.push((1, inc(pipe(7), title(7), 3 * HOUR)));
    v
}

fn repo_root(i: usize) -> PathBuf {
    let (name, parent, _) = REPOS[i];
    paths::normalize(&paths::home().join(parent).join(name))
}

pub fn catalog(now: i64) -> Catalog {
    let mut per_session: HashMap<usize, Vec<PeerLink>> = HashMap::new();
    for (i, l) in links(now) {
        per_session.entry(i).or_default().push(l);
    }
    let mut order: Vec<usize> = (0..SESSIONS.len()).collect();
    order.sort_by_key(|&i| SESSIONS[i].age);
    let mut repos: Vec<Repo> = REPOS
        .iter()
        .enumerate()
        .map(|(i, (name, _, git_age))| {
            let root = repo_root(i);
            Repo {
                key: paths::key(&root),
                root,
                name: (*name).to_string(),
                kind: RootKind::Git,
                exists: true,
                last_activity_ms: now - git_age,
                sessions: Vec::new(),
            }
        })
        .collect();
    let some = |s: &str| (!s.is_empty()).then(|| s.to_string());
    let mut sessions = Vec::new();
    for &i in &order {
        let d = &SESSIONS[i];
        let root = repo_root(d.repo);
        let mut links = per_session.remove(&i).unwrap_or_default();
        links.sort_by_key(|l| l.ts_ms);
        let mtime = now - d.age;
        repos[d.repo].sessions.push(sessions.len());
        repos[d.repo].last_activity_ms = repos[d.repo].last_activity_ms.max(mtime);
        sessions.push(Session {
            meta: SessionMeta {
                harness: d.harness,
                id: d.id.into(),
                file: PathBuf::from(format!("demo/{}.jsonl", d.id)),
                // opencode guarda sus sesiones en una base de datos: no hay archivo propio.
                size: if d.harness == Harness::Opencode { 0 } else { 40_000 + u64::from(d.prompts) * 18_500 },
                mtime_ms: mtime,
                cwd: Some(root.to_string_lossy().into_owned()),
                custom_title: Some(d.title.into()),
                first_prompt: Some(d.prompt.into()),
                last_prompt: Some(d.prompt.into()),
                git_branch: some(d.branch),
                entrypoint: some(d.surface),
                started_ms: Some(mtime - 2 * HOUR),
                pr_number: d.pr,
                model: some(d.model),
                prompts: d.prompts,
                has_messages: true,
                links,
                ..Default::default()
            },
            repo_key: paths::key(&root),
            launch_cwd: d.harness.launch_cwd(&root),
            cwd_exists: true,
            subpath: None,
            worktree: None,
        });
    }
    repos.sort_by(|a, b| b.last_activity_ms.cmp(&a.last_activity_ms).then(a.name.cmp(&b.name)));
    let by_id = sessions.iter().enumerate().map(|(i, s)| (s.meta.id.clone(), i)).collect();
    let repo_by_key = repos.iter().enumerate().map(|(i, r)| (r.key.clone(), i)).collect();
    let harnesses = Harness::ALL.into_iter().filter(|h| SESSIONS.iter().any(|d| d.harness == *h)).collect();
    Catalog { sessions, by_id, repos, repo_by_key, harnesses }
}

/// Sesiones vivas: (índice, estado, espera, subagentes). Las de Claude publican
/// su estado y su canal de mensajes; las demás son las que abrió claunch.
fn live(now: i64) -> HashMap<String, LiveSession> {
    let rows: [(usize, &str, Option<&str>, usize); 7] = [
        (0, "busy", None, 2),
        (1, "waiting", Some("input needed"), 0),
        (2, "busy", None, 3),
        (3, "idle", None, 0),
        (4, "idle", None, 0),
        (14, "busy", None, 0),
        (15, "idle", None, 0),
    ];
    rows.iter()
        .map(|&(i, status, waiting, subagents)| {
            let d = &SESSIONS[i];
            let claude = d.harness == Harness::Claude;
            let l = LiveSession {
                harness: d.harness,
                pid: 4120 + i as u32 * 1337,
                session_id: d.id.into(),
                cwd: repo_root(d.repo).to_string_lossy().into_owned(),
                entrypoint: if claude { d.surface.into() } else { "cli".into() },
                status: status.into(),
                waiting_for: waiting.map(Into::into),
                name: Some(d.title.into()),
                started_ms: now - 2 * HOUR,
                socket: claude.then(|| format!("demo-pipe-{i}")),
                bridge_id: None,
                subagents,
            };
            (d.id.to_string(), l)
        })
        .collect()
}

fn aliases() -> Aliases {
    SESSIONS.iter().enumerate().filter(|(_, d)| d.harness == Harness::Claude).map(|(i, d)| (pipe(i), d.id.to_string())).collect()
}

fn usage(now: i64) -> UsageState {
    let limit = |label: &str, short: &str, pct: f64, resets: i64, rank: u8| Limit {
        label: label.into(),
        short: short.into(),
        pct,
        resets_ms: Some(now + resets),
        active: true,
        rank,
    };
    UsageState::Ready(Usage {
        limits: vec![
            limit("sesión (5 h)", "5 h", 38.0, 2 * HOUR + 10 * MIN, 0),
            limit("semana", "semana", 61.0, 28 * HOUR, 1),
        ],
        plan: Some("max".into()),
        fetched_ms: now,
    })
}

fn sets() -> Vec<SessionSet> {
    let entry = |repo: usize, session: SessionRef, harness: Harness, color: &str| SetEntry {
        repo: repo_root(repo).to_string_lossy().into_owned(),
        cwd: None,
        session,
        harness,
        title: None,
        tab_color: Some(color.into()),
        options: LaunchOptions::default(),
    };
    vec![
        SessionSet {
            name: "Lanzamiento de la tienda".into(),
            layout: SetLayout::Panes,
            window: SetWindow::New,
            entries: vec![
                entry(0, SessionRef::Latest, Harness::Claude, "#D97757"),
                entry(1, SessionRef::Latest, Harness::Claude, "#7AAAE0"),
                entry(2, SessionRef::New, Harness::Claude, "#93C480"),
                entry(0, SessionRef::Latest, Harness::Codex, "#7FC4C0"),
            ],
        },
        SessionSet {
            name: "Guardia de infraestructura".into(),
            layout: SetLayout::Tabs,
            window: SetWindow::New,
            entries: vec![entry(3, SessionRef::Latest, Harness::Claude, "#E8B94F"), entry(4, SessionRef::New, Harness::Opencode, "#B997E0")],
        },
    ]
}

/// Rellena una `App` con datos de ejemplo (o los renueva para que el reloj
/// relativo y los mensajes «en vivo» sigan frescos).
pub fn refresh(app: &mut App) {
    let now = crate::util::now_ms();
    app.catalog = Arc::new(catalog(now));
    app.live = Arc::new(live(now));
    app.aliases = Arc::new(aliases());
    app.usage = usage(now);
}

pub fn app() -> App {
    let cfg = Config { sets: sets(), ..Default::default() };
    let general = cfg.general.clone();
    let mut app = App::new(cfg, None, None);
    app.demo = true;
    app.read_only = true;
    app.dry_run = Some(Vec::new());
    app.env = Ok(Env {
        // En la demo están todos los agentes, sin mirar qué hay instalado.
        commands: Harness::ALL.into_iter().map(|h| (h, vec![h.info().program.to_string()])).collect(),
        problems: Default::default(),
        git_bash: None,
        terminal: Terminal::Unavailable,
        self_exe: std::env::current_exe().unwrap_or_default(),
        general,
    });
    app.bypass = Ok(BypassStatus { enabled: false, mode: None, blocked_by_policy: false });
    app.toasts.clear();
    refresh(&mut app);
    app.loaded = true;
    app.sync_lists();
    app
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_is_consistent() {
        let app = app();
        let c = &app.catalog;
        assert_eq!(c.sessions.len(), SESSIONS.len());
        for s in &c.sessions {
            assert!(c.repo(&s.repo_key).is_some(), "{}", s.meta.title());
        }
        for id in app.live.keys() {
            assert!(c.session(id).is_some());
        }
        let g = crate::data::graph::build(c, &app.live, &app.aliases, crate::util::now_ms(), 24 * HOUR, 18);
        // Sesiones vivas (7) + auditoría + soporte + 2 remotas, sin duplicados.
        assert_eq!(g.nodes.len(), 11, "{:?}", g.nodes.iter().map(|n| &n.label).collect::<Vec<_>>());
        assert!(g.nodes.iter().all(|n| n.status != crate::data::graph::Status::Unknown));
        assert!(app.read_only && app.dry_run.is_some());
    }

    #[test]
    fn demo_shows_every_harness() {
        let app = app();
        assert_eq!(app.catalog.harnesses, Harness::ALL);
        assert_eq!(app.installed(), Harness::ALL);
        // Solo Claude habla entre sesiones: las demás no llevan mensajes.
        assert!(app.catalog.sessions.iter().all(|s| s.meta.harness == Harness::Claude || s.meta.links.is_empty()));
        // El set mezcla agentes y su «última» de Codex existe.
        let set = &app.cfg.sets[0];
        assert!(set.entries.iter().any(|e| e.harness == Harness::Codex));
        let codex_root = paths::key(&paths::expand(&set.entries[3].repo));
        assert!(app.catalog.sessions.iter().any(|s| s.meta.harness == Harness::Codex && paths::key(&s.launch_cwd) == codex_root));
    }
}
