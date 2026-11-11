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
use crate::launch::Env;
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
    id: &'static str,
    repo: usize,
    title: &'static str,
    prompt: &'static str,
    branch: &'static str,
    surface: &'static str,
    age: i64,
    prompts: u32,
    pr: Option<u64>,
}

/// Sesiones ficticias (la primera letra del id identifica la sesión en los mensajes).
const SESSIONS: [Demo; 14] = [
    Demo { id: "a1f0c2d4-7b3e-4c1a-9e2f-5d6b7a8c9e01", repo: 0, title: "Migrar cobros a la API nueva", prompt: "pasa los cobros recurrentes al endpoint v2 y avisa a la sesión del checkout", branch: "feat/cobros-v2", surface: "cli", age: 0, prompts: 34, pr: Some(128) },
    Demo { id: "b2e1d3c5-8a4f-4d2b-8f30-6e7c8b9daf02", repo: 1, title: "Rediseño del checkout", prompt: "adapta el checkout a la respuesta nueva de cobros", branch: "main", surface: "cli", age: MIN, prompts: 21, pr: None },
    Demo { id: "c3d2e4b6-9b5a-4e3c-a041-7f8d9cae0b03", repo: 2, title: "Notificaciones push de pedidos", prompt: "manda una push cuando el cobro quede confirmado", branch: "feat/push", surface: "claude-desktop", age: 2 * MIN, prompts: 17, pr: Some(64) },
    Demo { id: "d4c3f5a7-ac6b-4f4d-b152-8a9eadbf1c04", repo: 0, title: "Reintentos de webhooks", prompt: "los webhooks fallidos deben reintentarse con backoff", branch: "fix/webhooks", surface: "claude-vscode", age: 9 * MIN, prompts: 12, pr: None },
    Demo { id: "e5b4a6f8-bd7c-4a5e-8263-9bafbec02d05", repo: 3, title: "Revisión de alertas de producción", prompt: "revisa por qué saltó la alerta de latencia anoche", branch: "main", surface: "cli", age: 14 * MIN, prompts: 8, pr: None },
    Demo { id: "f6a5b7e9-ce8d-4b6f-9374-acb0cfd13e06", repo: 0, title: "Auditoría de seguridad de pagos", prompt: "audita el manejo de tarjetas y secretos", branch: "chore/auditoria", surface: "cli", age: 5 * HOUR, prompts: 26, pr: Some(121) },
    Demo { id: "a7f6c8da-df9e-4c70-a485-bdc1d0e24f07", repo: 1, title: "Accesibilidad del carrito", prompt: "revisa contraste y foco del carrito", branch: "feat/a11y", surface: "claude-vscode", age: 7 * HOUR, prompts: 15, pr: Some(97) },
    Demo { id: "b8a7d9eb-e0af-4d81-b596-ced2e1f35008", repo: 4, title: "Respuestas automáticas de soporte", prompt: "clasifica los tickets por urgencia", branch: "main", surface: "cli", age: 3 * HOUR, prompts: 19, pr: None },
    Demo { id: "c9b8eafc-f1b0-4e92-86a7-dfe3f2046109", repo: 5, title: "Guía de onboarding para clientes", prompt: "escribe la guía de primeros pasos", branch: "main", surface: "claude-desktop", age: 26 * HOUR, prompts: 11, pr: None },
    Demo { id: "dac9fb0d-02c1-4fa3-97b8-e0f4031572a0", repo: 6, title: "Panel de ventas semanal", prompt: "arma el panel con ventas por canal", branch: "main", surface: "cli", age: 50 * HOUR, prompts: 23, pr: None },
    Demo { id: "ebdafc1e-13d2-40b4-a8c9-f105142683b1", repo: 2, title: "Modo oscuro de la app", prompt: "agrega modo oscuro respetando el sistema", branch: "feat/dark", surface: "claude-vscode", age: 30 * HOUR, prompts: 9, pr: Some(61) },
    Demo { id: "fcebad2f-24e3-41c5-b9da-0216253794c2", repo: 3, title: "Costos de la nube de octubre", prompt: "resume los costos por servicio", branch: "main", surface: "cli", age: 3 * 24 * HOUR, prompts: 6, pr: None },
    Demo { id: "0dfcbe30-35f4-42d6-8aeb-1327364805d3", repo: 1, title: "Buscador de productos", prompt: "mejora la relevancia del buscador", branch: "feat/busqueda", surface: "cli", age: 4 * 24 * HOUR, prompts: 31, pr: Some(88) },
    Demo { id: "1e0dcf41-4605-43e7-9bfc-243847591ee4", repo: 4, title: "Plantillas de respuesta", prompt: "crea plantillas para devoluciones", branch: "main", surface: "claude-vscode", age: 5 * 24 * HOUR, prompts: 7, pr: None },
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
                id: d.id.into(),
                file: PathBuf::from(format!("demo/{}.jsonl", d.id)),
                size: 40_000 + u64::from(d.prompts) * 18_500,
                mtime_ms: mtime,
                cwd: Some(root.to_string_lossy().into_owned()),
                custom_title: Some(d.title.into()),
                first_prompt: Some(d.prompt.into()),
                last_prompt: Some(d.prompt.into()),
                git_branch: Some(d.branch.into()),
                entrypoint: Some(d.surface.into()),
                started_ms: Some(mtime - 2 * HOUR),
                pr_number: d.pr,
                prompts: d.prompts,
                has_messages: true,
                links,
                ..Default::default()
            },
            repo_key: paths::key(&root),
            launch_cwd: root,
            cwd_exists: true,
            subpath: None,
            worktree: None,
        });
    }
    repos.sort_by(|a, b| b.last_activity_ms.cmp(&a.last_activity_ms).then(a.name.cmp(&b.name)));
    let by_id = sessions.iter().enumerate().map(|(i, s)| (s.meta.id.clone(), i)).collect();
    let repo_by_key = repos.iter().enumerate().map(|(i, r)| (r.key.clone(), i)).collect();
    Catalog { sessions, by_id, repos, repo_by_key }
}

/// Sesiones vivas: (índice, estado, espera, superficie, subagentes).
fn live(now: i64) -> HashMap<String, LiveSession> {
    let rows: [(usize, &str, Option<&str>, usize); 5] =
        [(0, "busy", None, 2), (1, "waiting", Some("input needed"), 0), (2, "busy", None, 3), (3, "idle", None, 0), (4, "idle", None, 0)];
    rows.iter()
        .map(|&(i, status, waiting, subagents)| {
            let d = &SESSIONS[i];
            let l = LiveSession {
                pid: 4120 + i as u32 * 1337,
                session_id: d.id.into(),
                cwd: repo_root(d.repo).to_string_lossy().into_owned(),
                entrypoint: d.surface.into(),
                status: status.into(),
                waiting_for: waiting.map(Into::into),
                name: Some(d.title.into()),
                started_ms: now - 2 * HOUR,
                socket: Some(format!("demo-pipe-{i}")),
                bridge_id: None,
                subagents,
            };
            (d.id.to_string(), l)
        })
        .collect()
}

fn aliases() -> Aliases {
    SESSIONS.iter().enumerate().map(|(i, d)| (pipe(i), d.id.to_string())).collect()
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
    let entry = |repo: usize, session: SessionRef, color: &str| SetEntry {
        repo: repo_root(repo).to_string_lossy().into_owned(),
        cwd: None,
        session,
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
                entry(0, SessionRef::Latest, "#D97757"),
                entry(1, SessionRef::Latest, "#7AAAE0"),
                entry(2, SessionRef::New, "#93C480"),
            ],
        },
        SessionSet {
            name: "Guardia de infraestructura".into(),
            layout: SetLayout::Tabs,
            window: SetWindow::New,
            entries: vec![entry(3, SessionRef::Latest, "#E8B94F"), entry(4, SessionRef::New, "#B997E0")],
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
    let mut cfg = Config::default();
    cfg.sets = sets();
    let general = cfg.general.clone();
    let mut app = App::new(cfg, None, None);
    app.demo = true;
    app.read_only = true;
    app.dry_run = Some(Vec::new());
    app.env = Ok(Env {
        claude: vec!["claude".into()],
        git_bash: None,
        wt: None,
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
        for (id, _) in app.live.iter() {
            assert!(c.session(id).is_some());
        }
        let g = crate::data::graph::build(c, &app.live, &app.aliases, crate::util::now_ms(), 24 * HOUR, 18);
        // Sesiones vivas + auditoría + soporte + 2 remotas, sin duplicados.
        assert_eq!(g.nodes.len(), 9, "{:?}", g.nodes.iter().map(|n| &n.label).collect::<Vec<_>>());
        assert!(g.nodes.iter().all(|n| n.status != crate::data::graph::Status::Unknown));
        assert!(app.read_only && app.dry_run.is_some());
    }
}
