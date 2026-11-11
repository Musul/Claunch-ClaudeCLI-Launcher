//! Grafo de sesiones: quién habla con quién.
//!
//! Nodos: sesiones vivas de esta máquina y todo extremo que haya intercambiado
//! mensajes en la ventana elegida (sesiones locales ya cerradas, sesiones de
//! otras máquinas por Remote Control). Aristas: mensajes `SendMessage` y los
//! recibidos con `origin.kind = "peer"`. Un mensaje entre dos sesiones locales
//! queda en ambos historiales, así que cada sentido cuenta el máximo de los dos
//! lados en vez de sumarlos.

use std::collections::HashMap;

use super::{Aliases, Catalog, LiveSession};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    Busy,
    Waiting,
    Idle,
    /// Sesión local sin proceso vivo.
    Closed,
    /// Otra máquina (Remote Control): no se conoce su estado.
    Remote,
    /// Extremo que no se pudo identificar.
    Unknown,
}

impl Status {
    /// Grupo de orden en el mapa: vivas, cerradas, remotas, sin identificar.
    fn group(self) -> u8 {
        match self {
            Status::Busy | Status::Waiting | Status::Idle => 0,
            Status::Closed => 1,
            Status::Remote => 2,
            Status::Unknown => 3,
        }
    }
    pub fn is_live(self) -> bool {
        self.group() == 0
    }
}

#[derive(Debug, Clone)]
pub struct Node {
    pub key: String,
    pub label: String,
    /// Repo (local) o «otra máquina».
    pub sub: String,
    pub status: Status,
    pub session_id: Option<String>,
    pub subagents: usize,
    pub last_ms: i64,
}

#[derive(Debug, Clone)]
pub struct Edge {
    pub from: usize,
    pub to: usize,
    pub count: u32,
    pub last_ms: i64,
}

#[derive(Debug, Clone, Default)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    /// Nodos que no caben y se omitieron (los de actividad más antigua).
    pub hidden: usize,
}

impl Graph {
    pub fn index_of(&self, key: &str) -> Option<usize> {
        self.nodes.iter().position(|n| n.key == key)
    }
}

fn session_key(id: &str) -> String {
    format!("s:{id}")
}

#[derive(Default)]
struct Side {
    out: u32,
    inc: u32,
    last: i64,
}

/// Construye el grafo con los mensajes de los últimos `window_ms`.
pub fn build(catalog: &Catalog, live: &HashMap<String, LiveSession>, aliases: &Aliases, now_ms: i64, window_ms: i64, max_nodes: usize) -> Graph {
    let since = now_ms - window_ms;
    let mut aliases = aliases.clone();
    let mut by_name: HashMap<String, String> = HashMap::new();
    for l in live.values() {
        if let Some(s) = &l.socket {
            aliases.insert(format!("uds:{s}"), l.session_id.clone());
        }
        if let Some(b) = &l.bridge_id {
            aliases.insert(format!("bridge:{b}"), l.session_id.clone());
        }
        if let Some(n) = &l.name {
            by_name.insert(n.clone(), session_key(&l.session_id));
        }
    }
    // Nombres puestos por el usuario (`/rename`, `-n`): la más reciente gana.
    for s in &catalog.sessions {
        for n in [s.meta.custom_title.as_ref(), s.meta.agent_name.as_ref()].into_iter().flatten() {
            by_name.entry(n.clone()).or_insert_with(|| session_key(&s.meta.id));
        }
    }
    // Nombres de remitentes vistos (para destinos escritos por nombre).
    let mut names: HashMap<String, String> = HashMap::new();
    for s in &catalog.sessions {
        for l in s.meta.links.iter().filter(|l| !l.out && l.ts_ms >= since) {
            let key = aliases.get(&l.peer).map(|id| session_key(id)).unwrap_or_else(|| l.peer.clone());
            if let Some(n) = l.name.as_ref().filter(|n| !n.is_empty()) {
                names.entry(key.clone()).or_insert_with(|| n.clone());
                by_name.entry(n.clone()).or_insert(key);
            }
        }
    }
    // Un proceso nuevo de la misma sesión (al reanudar) estrena tubería: si la
    // tubería no se conoce, el nombre del remitente la identifica. Lo aprendido
    // de los recibidos sirve también para los enviados a esa tubería (sin nombre).
    let mut learned: HashMap<String, String> = HashMap::new();
    for s in &catalog.sessions {
        for l in s.meta.links.iter().filter(|l| !l.out && l.peer.starts_with("uds:") && !aliases.contains_key(&l.peer)) {
            if let Some(k) = l.name.as_ref().and_then(|n| by_name.get(n)).filter(|k| k.starts_with("s:")) {
                learned.entry(l.peer.clone()).or_insert_with(|| k.clone());
            }
        }
    }
    let resolve = |peer: &str, name: Option<&str>| -> String {
        if let Some(id) = aliases.get(peer) {
            return session_key(id);
        }
        if let Some(k) = learned.get(peer) {
            return k.clone();
        }
        if peer.starts_with("uds:") {
            return name
                .and_then(|n| by_name.get(n))
                .filter(|k| k.starts_with("s:"))
                .cloned()
                .unwrap_or_else(|| peer.to_string());
        }
        if peer.starts_with("bridge:") {
            return peer.to_string();
        }
        by_name.get(peer).cloned().unwrap_or_else(|| format!("name:{peer}"))
    };

    let mut sides: HashMap<(String, String), Side> = HashMap::new();
    for s in &catalog.sessions {
        let me = session_key(&s.meta.id);
        for l in s.meta.links.iter().filter(|l| l.ts_ms >= since) {
            let other = resolve(&l.peer, l.name.as_deref());
            if other == me {
                continue;
            }
            let pair = if l.out { (me.clone(), other) } else { (other, me.clone()) };
            let side = sides.entry(pair).or_default();
            if l.out { side.out += 1 } else { side.inc += 1 }
            side.last = side.last.max(l.ts_ms);
        }
    }

    // Nodos: vivos + extremos con mensajes.
    let mut last: HashMap<String, i64> = HashMap::new();
    for l in live.values() {
        last.entry(session_key(&l.session_id)).or_insert(0);
    }
    for ((a, b), side) in &sides {
        for k in [a, b] {
            let e = last.entry(k.clone()).or_insert(0);
            *e = (*e).max(side.last);
        }
    }
    let mut nodes: Vec<Node> = last
        .into_iter()
        .map(|(key, last_ms)| node(&key, last_ms, catalog, live, &names))
        .collect();
    nodes.sort_by(|a, b| {
        b.status.is_live().cmp(&a.status.is_live()).then(b.last_ms.cmp(&a.last_ms)).then(a.label.cmp(&b.label))
    });
    let hidden = nodes.len().saturating_sub(max_nodes);
    nodes.truncate(max_nodes);
    // Orden estable en pantalla: agrupa por repo para que no salte al refrescar.
    nodes.sort_by(|a, b| a.status.group().cmp(&b.status.group()).then(a.sub.cmp(&b.sub)).then(a.label.cmp(&b.label)).then(a.key.cmp(&b.key)));

    let pos: HashMap<&str, usize> = nodes.iter().enumerate().map(|(i, n)| (n.key.as_str(), i)).collect();
    let mut edges: Vec<Edge> = sides
        .iter()
        .filter_map(|((a, b), side)| {
            Some(Edge { from: *pos.get(a.as_str())?, to: *pos.get(b.as_str())?, count: side.out.max(side.inc), last_ms: side.last })
        })
        .collect();
    edges.sort_by_key(|e| (e.from, e.to));
    Graph { nodes, edges, hidden }
}

fn node(key: &str, last_ms: i64, catalog: &Catalog, live: &HashMap<String, LiveSession>, names: &HashMap<String, String>) -> Node {
    if let Some(id) = key.strip_prefix("s:") {
        let l = live.get(id);
        let s = catalog.session(id);
        let label = s
            .map(|s| s.meta.title().to_string())
            .or_else(|| l.and_then(|l| l.name.clone()))
            .or_else(|| names.get(key).cloned())
            .unwrap_or_else(|| format!("sesión {}", &id[..id.len().min(8)]));
        let sub = s
            .and_then(|s| catalog.repo(&s.repo_key))
            .map(|r| r.name.clone())
            .or_else(|| l.and_then(|l| std::path::Path::new(&l.cwd).file_name().map(|n| n.to_string_lossy().into_owned())))
            .unwrap_or_default();
        let status = match l {
            Some(l) if l.is_busy() => Status::Busy,
            Some(l) if l.is_waiting() => Status::Waiting,
            Some(_) => Status::Idle,
            None => Status::Closed,
        };
        let mut last_ms = last_ms;
        if let Some(s) = s {
            last_ms = last_ms.max(s.meta.mtime_ms);
        }
        return Node {
            key: key.to_string(),
            label,
            sub,
            status,
            session_id: Some(id.to_string()),
            subagents: l.map(|l| l.subagents).unwrap_or(0),
            last_ms,
        };
    }
    let (label, sub, status) = if let Some(b) = key.strip_prefix("bridge:") {
        let short = b.trim_start_matches("session_");
        (names.get(key).cloned().unwrap_or_else(|| format!("remota {}", &short[..short.len().min(8)])), "otra máquina".to_string(), Status::Remote)
    } else if key.starts_with("uds:") {
        (names.get(key).cloned().unwrap_or_else(|| "sesión local".into()), "cerrada".to_string(), Status::Closed)
    } else {
        let n = key.strip_prefix("name:").unwrap_or(key);
        (n.to_string(), "sin identificar".to_string(), Status::Unknown)
    };
    Node { key: key.to_string(), label, sub, status, session_id: None, subagents: 0, last_ms }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::SessionMeta;
    use crate::data::sessions::PeerLink;
    use std::path::PathBuf;

    fn meta(id: &str, links: Vec<PeerLink>) -> SessionMeta {
        SessionMeta {
            id: id.into(),
            file: PathBuf::from(format!("{id}.jsonl")),
            cwd: Some(std::env::temp_dir().to_string_lossy().into_owned()),
            has_messages: true,
            mtime_ms: 1,
            links,
            ..Default::default()
        }
    }

    fn link(out: bool, peer: &str, name: Option<&str>, ts: i64) -> PeerLink {
        PeerLink { out, peer: peer.into(), name: name.map(Into::into), id: None, ts_ms: ts }
    }

    #[test]
    fn dedupes_local_pairs_and_resolves_aliases() {
        // A envía 2 a B (por tubería); B registra 2 recibidos de la tubería de A.
        let a = meta("A", vec![link(true, "uds:pipe-b", None, 100), link(true, "uds:pipe-b", None, 200)]);
        let b = meta("B", vec![link(false, "uds:pipe-a", Some("sesión A"), 100), link(false, "uds:pipe-a", Some("sesión A"), 200)]);
        let catalog = Catalog::build(&[a, b], &[]);
        let mut live = HashMap::new();
        live.insert("B".to_string(), LiveSession { session_id: "B".into(), status: "busy".into(), socket: Some("pipe-b".into()), ..Default::default() });
        let mut aliases = Aliases::new();
        aliases.insert("uds:pipe-a".into(), "A".into());
        let g = build(&catalog, &live, &aliases, 1_000, 10_000, 20);
        assert_eq!(g.nodes.len(), 2, "{:?}", g.nodes);
        assert_eq!(g.edges.len(), 1);
        assert_eq!(g.edges[0].count, 2);
        let from = &g.nodes[g.edges[0].from];
        assert_eq!(from.session_id.as_deref(), Some("A"));
        assert_eq!(from.status, Status::Closed);
        assert_eq!(g.nodes[g.edges[0].to].status, Status::Busy);
    }

    #[test]
    fn remote_peers_and_window() {
        let a = meta(
            "A",
            vec![
                link(false, "bridge:session_01XYZ", Some("Directorio"), 900),
                link(true, "Directorio", None, 950),
                link(true, "bridge:session_old", None, 10),
            ],
        );
        let catalog = Catalog::build(&[a], &[]);
        let g = build(&catalog, &HashMap::new(), &Aliases::new(), 1_000, 500, 20);
        // El viejo queda fuera de la ventana; «Directorio» se resuelve al remoto.
        assert_eq!(g.nodes.len(), 2, "{:?}", g.nodes);
        assert_eq!(g.edges.len(), 2);
        let remote = g.nodes.iter().find(|n| n.status == Status::Remote).unwrap();
        assert_eq!(remote.label, "Directorio");
    }
}
