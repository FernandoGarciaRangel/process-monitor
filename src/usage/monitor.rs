//! A thread `usage`: a cada [`TICK`] lê as fontes locais, decide se vai à rede,
//! escolhe a leitura mais nova de cada provedor, atualiza o ritmo e avalia os alertas.
//! Fica viva com a janela na bandeja, porque os alertas dependem dela. O consumo por
//! projeto (os `.jsonl`) só é lido com a tela "IA" aberta.
//!
//! Os caminhos vêm de [`Paths::detect`] (variáveis do Windows, `CLAUDE_CONFIG_DIR`,
//! `CODEX_HOME`). Sem Claude Code, sem Codex, sem Codenotch, sem statusline e sem rede,
//! a rodada termina com notas explicando, nunca com erro. Um pânico numa rodada (um
//! arquivo num formato que ninguém previu) é contido: a thread segue na próxima.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::gate::Gate;
use super::{
    Consumption, FIVE_HOUR, FIVE_HOURS_SECS, Limits, Paths, Provider, ProviderState, SEVEN_DAY, SEVEN_DAYS_SECS, Source, Usage,
    aggregate, bridge, claude_api, claude_logs, codenotch, codex_api, codex_logs,
};
use crate::alerts::{self, UsageAlerts, UsageWindow};
use crate::sampler::{AlertSink, Series};

const TICK: Duration = Duration::from_secs(30);
/// Pontos do gráfico: 2 h a cada 30 s.
const HISTORY_POINTS: usize = 240;
/// Pontos guardados para o ritmo (últimas 2 h).
const PACE_SECS: i64 = 2 * 3600;
/// Depois de uma falha do `codex app-server`, ele só é tentado de novo depois disso.
const APP_SERVER_STAND_DOWN: i64 = 30 * 60;

enum Msg {
    Active(bool),
    Network(bool),
    Refresh,
    Alerts(alerts::Config),
    AlertSink(AlertSink),
}

pub struct Monitor {
    latest: Arc<Mutex<Arc<Usage>>>,
    tx: Sender<Msg>,
}

impl Monitor {
    pub fn spawn(ctx: egui::Context, network: bool) -> Self {
        let latest = Arc::new(Mutex::new(Arc::new(Usage { network, ..Usage::default() })));
        let (tx, rx) = mpsc::channel();
        let shared = latest.clone();
        std::thread::Builder::new()
            .name("usage".into())
            .spawn(move || run(ctx, shared, rx, network))
            .expect("thread de uso de IA");
        Self { latest, tx }
    }

    pub fn latest(&self) -> Arc<Usage> {
        self.latest.lock().map(|u| u.clone()).unwrap_or_default()
    }

    /// Com a tela "IA" aberta, lê também o consumo por projeto.
    pub fn set_active(&self, on: bool) {
        let _ = self.tx.send(Msg::Active(on));
    }

    /// Consulta própria aos servidores (não oficial).
    pub fn set_network(&self, on: bool) {
        let _ = self.tx.send(Msg::Network(on));
    }

    /// Relê as fontes já. A rede continua respeitando o piso de 5 min.
    pub fn refresh_now(&self) {
        let _ = self.tx.send(Msg::Refresh);
    }

    pub fn set_alerts(&self, config: alerts::Config) {
        let _ = self.tx.send(Msg::Alerts(config));
    }

    pub fn set_alert_sink(&self, sink: AlertSink) {
        let _ = self.tx.send(Msg::AlertSink(sink));
    }
}

/// Estado da consulta própria de um provedor, gravado em disco (sem token).
#[derive(Default)]
struct Net {
    gate: Gate,
    /// Última leitura boa da rede.
    reading: Option<Limits>,
    /// Por que a última tentativa não deu leitura.
    note: Option<String>,
    /// Codex: quando o `app-server` pode ser tentado de novo.
    app_server_after: i64,
}

impl Net {
    fn to_json(&self) -> Value {
        serde_json::json!({
            "gate": self.gate.to_json(),
            "reading": self.reading.as_ref().map(Limits::to_json),
            "note": self.note,
            "app_server_after": self.app_server_after,
        })
    }

    fn from_json(v: &Value) -> Self {
        Self {
            gate: v.get("gate").map(Gate::from_json).unwrap_or_default(),
            reading: v.get("reading").and_then(Limits::from_json),
            note: v.get("note").and_then(Value::as_str).map(str::to_owned),
            app_server_after: v.get("app_server_after").and_then(Value::as_i64).unwrap_or(0),
        }
    }

    fn resets(&self) -> Vec<i64> {
        self.reading.iter().flat_map(|l| l.windows.iter().filter_map(|w| w.resets_at)).collect()
    }
}

struct State {
    path: PathBuf,
    claude: Net,
    codex: Net,
    alerts: UsageAlerts,
}

impl State {
    /// Arquivo ausente ou corrompido: começa do zero.
    fn load(path: PathBuf) -> Self {
        let v = super::read_json(&path).unwrap_or(Value::Null);
        Self {
            path,
            claude: v.get("claude").map(Net::from_json).unwrap_or_default(),
            codex: v.get("codex").map(Net::from_json).unwrap_or_default(),
            alerts: v.get("alerts").map(UsageAlerts::from_json).unwrap_or_default(),
        }
    }

    fn save(&self) {
        let v = serde_json::json!({
            "claude": self.claude.to_json(), "codex": self.codex.to_json(), "alerts": self.alerts.to_json(),
        });
        if let Err(e) = super::write_atomic(&self.path, &v.to_string())
            && cfg!(debug_assertions)
        {
            eprintln!("[ia] não gravou o estado: {e}");
        }
    }
}

/// Pontos (hora, uso) de cada janela, para o ritmo e o gráfico.
#[derive(Default)]
struct Trail {
    points: HashMap<String, VecDeque<(i64, f32)>>,
    series: HashMap<String, Series>,
    /// `resets_at` de cada janela: mudou, a série recomeça (é outra janela).
    resets: HashMap<String, Option<i64>>,
}

impl Trail {
    fn record(&mut self, limits: &mut Limits, now: i64) {
        for w in &mut limits.windows {
            let reset = w.resets_at;
            let same = self.resets.get(&w.id).is_none_or(|old| match (old, reset) {
                (Some(a), Some(b)) => (a - b).abs() <= 30 * 60,
                _ => true,
            });
            if !same {
                self.points.remove(&w.id);
                self.series.remove(&w.id);
            }
            self.resets.insert(w.id.clone(), reset);
            if w.expired(now) {
                continue;
            }
            let points = self.points.entry(w.id.clone()).or_default();
            // O ponto é da hora da leitura, não da rodada: a mesma leitura lida de novo
            // não pode parecer uso parado.
            if points.back().is_none_or(|(t, _)| *t < limits.fetched_at) {
                points.push_back((limits.fetched_at, w.used));
            }
            while points.front().is_some_and(|(t, _)| now.saturating_sub(*t) > PACE_SECS) {
                points.pop_front();
            }
            let pts: Vec<(i64, f32)> = points.iter().copied().collect();
            w.pace = super::forecast(w, &pts, now);
            w.rate = super::rate(w, &pts, now);
            self.series.entry(w.id.clone()).or_insert_with(|| Series::with_capacity(HISTORY_POINTS)).push(w.used);
        }
    }

    fn history(&self) -> Vec<(String, Series)> {
        let mut out: Vec<(String, Series)> = self.series.iter().map(|(k, s)| (k.clone(), s.clone())).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

/// A leitura mais nova entre as que têm janelas. Uma com data no futuro (relógio errado,
/// arquivo corrompido) ganharia para sempre: fica de fora.
fn freshest(now: i64, candidates: impl IntoIterator<Item = Option<Limits>>) -> Option<Limits> {
    candidates
        .into_iter()
        .flatten()
        .filter(|l| !l.windows.is_empty() && l.fetched_at <= now.saturating_add(86_400))
        .max_by_key(|l| (l.fetched_at, rank(l.source)))
}

/// Empate de horário: a fonte mais confiável ganha.
fn rank(s: Source) -> u8 {
    match s {
        Source::Api => 4,
        Source::AppServer => 3,
        Source::StatusLine => 2,
        Source::Codenotch => 1,
        Source::CodexLog => 0,
    }
}

/// Tudo o que a thread guarda entre rodadas.
struct Worker {
    paths: Paths,
    state: State,
    alert_config: alerts::Config,
    sink: Option<AlertSink>,
    active: bool,
    network: bool,
    claude_logs: claude_logs::Scanner,
    codex_logs: codex_logs::Scanner,
    scanned: bool,
    trails: HashMap<Provider, Trail>,
    /// O `codex.exe` existe nesta máquina (procurado uma vez: varre o PATH).
    codex_exe: bool,
}

impl Worker {
    fn new(paths: Paths, network: bool) -> Self {
        let codex_exe = codex_api::find_codex(&paths).is_some();
        Self {
            state: State::load(paths.state_file.clone()),
            paths,
            alert_config: alerts::Config::default(),
            sink: None,
            active: false,
            network,
            claude_logs: claude_logs::Scanner::default(),
            codex_logs: codex_logs::Scanner::default(),
            scanned: false,
            trails: HashMap::new(),
            codex_exe,
        }
    }

    /// Uma rodada. Pública para os testes, que a rodam com pastas sintéticas.
    fn tick(&mut self, now: i64) -> Usage {
        let paths = self.paths.clone();
        let is_dir = |p: &Option<PathBuf>| p.as_ref().is_some_and(|p| p.is_dir());
        let claude_installed = is_dir(&paths.claude_dir);
        let codex_installed = is_dir(&paths.codex_home) || self.codex_exe;

        // Codex CLI: barato (só as pastas dos últimos dias) e traz a % de cada rodada.
        let codex_sessions = paths.codex_sessions();
        let codex_has_logs = is_dir(&codex_sessions);
        if let Some(root) = codex_sessions.as_ref().filter(|_| codex_has_logs) {
            self.codex_logs.update(root, now);
        }
        let claude_projects = paths.claude_projects();
        let claude_has_logs = is_dir(&claude_projects);
        if self.active {
            if let Some(root) = claude_projects.as_ref().filter(|_| claude_has_logs) {
                self.claude_logs.update(root, now);
            }
            self.scanned = true;
        }

        // Claude.
        let notch = codenotch::read(paths.codenotch_dir.as_deref(), Provider::Claude);
        let line = bridge::read(paths.bridge_file.as_deref());
        let busy = notch.holds_network(now) || line.as_ref().is_some_and(|l| now - l.fetched_at < bridge::FRESH_SECS);
        if self.network && !busy && self.state.claude.gate.may_fetch(now, &self.state.claude.resets()) {
            fetch_claude(&mut self.state.claude, paths.claude_dir.as_deref(), now);
            self.state.save();
        }
        let mut claude_note = if self.network { self.state.claude.note.clone().filter(|_| !busy) } else { None };
        let network_reading = self.network.then(|| self.state.claude.reading.clone()).flatten();
        let mut claude = freshest(now, [notch.limits, line, network_reading]);

        // Codex.
        let notch = codenotch::read(paths.codenotch_dir.as_deref(), Provider::Codex);
        let rollout = self.codex_logs.limits().cloned();
        let busy = notch.holds_network(now);
        if self.network && !busy && self.state.codex.gate.may_fetch(now, &self.state.codex.resets()) {
            fetch_codex(&mut self.state.codex, &paths, now);
            self.state.save();
        }
        let mut codex_note = if self.network { self.state.codex.note.clone().filter(|_| !busy) } else { None };
        let network_reading = self.network.then(|| self.state.codex.reading.clone()).flatten();
        let mut codex = freshest(now, [notch.limits, rollout, network_reading]);

        if claude.is_none() && claude_note.is_none() {
            claude_note = Some(no_source_note(Provider::Claude, claude_installed, self.network));
        }
        if codex.is_none() && codex_note.is_none() {
            codex_note = Some(no_source_note(Provider::Codex, codex_installed, self.network));
        }

        for (provider, limits) in [(Provider::Claude, &mut claude), (Provider::Codex, &mut codex)] {
            if let Some(l) = limits {
                self.trails.entry(provider).or_default().record(l, now);
            }
        }

        // Alertas.
        let windows: Vec<UsageWindow> = [(Provider::Claude, &claude), (Provider::Codex, &codex)]
            .into_iter()
            .flat_map(|(p, l)| l.iter().flat_map(move |l| l.windows.iter().map(move |w| (p, w))))
            .map(|(p, w)| UsageWindow {
                provider: p.label().to_owned(),
                id: w.id.clone(),
                phrase: super::window_phrase(&w.id, w.duration_secs),
                used: w.used,
                resets_at: w.resets_at,
            })
            .collect();
        if let Some(sink) = &self.sink {
            let before = self.state.alerts.clone();
            for alert in self.state.alerts.evaluate(now, &windows, &self.alert_config) {
                sink(&alert);
            }
            if self.state.alerts != before {
                self.state.save();
            }
        }

        let (claude_recent, claude_week) =
            if self.scanned { periods(self.claude_logs.records(), &claude, now) } else { Default::default() };
        let (codex_recent, codex_week) = periods(self.codex_logs.records(), &codex, now);

        Usage {
            claude: ProviderState {
                limits: claude,
                note: claude_note,
                history: self.trails.get(&Provider::Claude).map(Trail::history).unwrap_or_default(),
                recent: self.scanned.then_some(claude_recent),
                week: self.scanned.then_some(claude_week),
                has_logs: claude_has_logs,
                installed: claude_installed,
            },
            codex: ProviderState {
                limits: codex,
                note: codex_note,
                history: self.trails.get(&Provider::Codex).map(Trail::history).unwrap_or_default(),
                recent: codex_has_logs.then_some(codex_recent),
                week: codex_has_logs.then_some(codex_week),
                has_logs: codex_has_logs,
                installed: codex_installed,
            },
            network: self.network,
            scanned: self.scanned,
        }
    }
}

/// Consumo da janela de 5 h e da semana. Com a janela conhecida, o período começa nela;
/// sem, são as últimas 5 h e os últimos 7 dias.
fn periods(records: &[super::Record], limits: &Option<Limits>, now: i64) -> (Consumption, Consumption) {
    let start = |id: &str, span: i64| match limits.as_ref().and_then(|l| l.window(id)).filter(|w| !w.expired(now)).and_then(|w| w.starts_at()) {
        Some(s) => (s, true),
        None => (now - span, false),
    };
    let (recent, aligned_recent) = start(FIVE_HOUR, FIVE_HOURS_SECS);
    let (week, aligned_week) = start(SEVEN_DAY, SEVEN_DAYS_SECS);
    let mut out = (aggregate(records.iter(), recent, aligned_recent), aggregate(records.iter(), week, aligned_week));
    // Confere aqui, não a cada quadro na interface: a pasta pode ter sido apagada ou
    // estar num disco que não está mais ligado.
    for c in [&mut out.0, &mut out.1] {
        for t in &mut c.top {
            t.folder_exists = t.project != "?" && Path::new(&t.project).is_dir();
        }
    }
    out
}

fn run(ctx: egui::Context, latest: Arc<Mutex<Arc<Usage>>>, rx: Receiver<Msg>, network: bool) {
    let mut worker = Worker::new(Paths::detect(), network);
    // Espera um pouco antes da primeira rodada: a abertura é da coleta.
    let mut deadline = Instant::now() + Duration::from_secs(2);

    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(msg) => {
                let collect_now = matches!(msg, Msg::Refresh | Msg::Network(_) | Msg::Active(true));
                match msg {
                    Msg::Active(on) => worker.active = on,
                    Msg::Network(on) => worker.network = on,
                    Msg::Refresh => worker.codex_exe = codex_api::find_codex(&worker.paths).is_some(),
                    Msg::Alerts(c) => worker.alert_config = c,
                    Msg::AlertSink(s) => worker.sink = Some(s),
                }
                // O resto só aplica e continua esperando a rodada.
                if !collect_now {
                    continue;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        deadline = Instant::now() + TICK;
        let now = super::now_secs();
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker.tick(now))) {
            Ok(usage) => {
                if let Ok(mut slot) = latest.lock() {
                    *slot = Arc::new(usage);
                }
                ctx.request_repaint();
            }
            // Um registro num formato imprevisto não pode matar a thread (e os alertas com
            // ela). Recomeça a leitura dos registros, que é o que guarda estado de arquivo.
            Err(_) => {
                worker.claude_logs = claude_logs::Scanner::default();
                worker.codex_logs = codex_logs::Scanner::default();
            }
        }
    }
}

fn no_source_note(p: Provider, installed: bool, network: bool) -> String {
    match (p, installed, network) {
        (Provider::Claude, false, _) => {
            "Claude Code não encontrado neste computador (nem a pasta .claude do usuário, nem CLAUDE_CONFIG_DIR). \
             Com o Codenotch aberto, a % aparece mesmo assim."
                .to_owned()
        }
        (Provider::Codex, false, _) => {
            "Codex não encontrado neste computador (nem a pasta .codex do usuário, nem CODEX_HOME, nem o codex.exe). \
             Com o Codenotch aberto, a % aparece mesmo assim."
                .to_owned()
        }
        (Provider::Claude, true, false) => {
            "Sem leitura ainda. Com a statusline configurada (veja o README), o Claude Code grava a % a cada resposta; \
             com o Codenotch aberto, ele também serve. A consulta própria fica nas configurações."
                .to_owned()
        }
        (Provider::Codex, true, false) => {
            "Sem leitura ainda. Com o Codenotch aberto ou usando o Codex CLI, a % aparece aqui. A consulta própria fica nas \
             configurações."
                .to_owned()
        }
        (_, true, true) => "Esperando a primeira leitura.".to_owned(),
    }
}

/// Uma tentativa no servidor do Claude. O token nunca sai de `claude_api`.
fn fetch_claude(net: &mut Net, dir: Option<&Path>, now: i64) {
    use claude_api::Error;
    let result = match dir {
        Some(dir) => claude_api::fetch(dir, now),
        None => Err(Error::NoCredential),
    };
    // Sem credencial ou com ela vencida, nada vai à rede: não conta como tentativa.
    match result {
        Ok(l) => {
            net.gate.attempt(now);
            net.gate.succeeded();
            net.reading = Some(l);
            net.note = None;
        }
        Err(e @ (Error::NoCredential | Error::Expired)) => net.note = Some(capitalize(&e.describe())),
        Err(Error::RateLimited(after)) => {
            net.gate.attempt(now);
            let wait = net.gate.rate_limited(now, after);
            net.note = Some(format!("O servidor pediu para esperar; nova tentativa {}.", crate::format::when(now + wait, now)));
        }
        Err(e) => {
            net.gate.attempt(now);
            net.note = Some(capitalize(&e.describe()));
        }
    }
    if cfg!(debug_assertions) {
        eprintln!("[ia] claude: {}", net.note.as_deref().unwrap_or("leitura ok"));
    }
}

/// `wham/usage` e, se falhar sem ser por 429, o `codex app-server`.
fn fetch_codex(net: &mut Net, paths: &Paths, now: i64) {
    use codex_api::Error;
    let direct = match &paths.codex_home {
        Some(home) => codex_api::fetch(home, now),
        None => Err(Error::NoCredential),
    };
    let tried = !matches!(direct, Err(Error::NoCredential | Error::Expired));
    if tried {
        net.gate.attempt(now);
    }
    match direct {
        Ok(l) => {
            net.gate.succeeded();
            net.reading = Some(l);
            net.note = None;
        }
        Err(Error::RateLimited(after)) => {
            let wait = net.gate.rate_limited(now, after);
            net.note = Some(format!("O servidor pediu para esperar; nova tentativa {}.", crate::format::when(now + wait, now)));
        }
        Err(e) => {
            // O app-server não serve para driblar um 429; para os outros erros, ele usa o
            // login do Codex do jeito dele.
            if net.app_server_after <= now {
                net.gate.attempt(now);
                match codex_api::read_app_server(paths, now) {
                    Some(l) => {
                        net.reading = Some(l);
                        net.note = None;
                    }
                    None => {
                        net.app_server_after = now + APP_SERVER_STAND_DOWN;
                        net.note = Some(capitalize(&e.describe()));
                    }
                }
            } else {
                net.note = Some(capitalize(&e.describe()));
            }
        }
    }
    if cfg!(debug_assertions) {
        eprintln!("[ia] codex: {}", net.note.as_deref().unwrap_or("leitura ok"));
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str() + if s.ends_with('.') { "" } else { "." },
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{Kind, Window, claude_logs::tests::LINE};

    const T: i64 = 1_800_000_000;

    fn limits(source: Source, at: i64) -> Option<Limits> {
        Some(Limits { windows: vec![Window::new(FIVE_HOUR, 10.0, None, None)], source, fetched_at: at, plan: None })
    }

    /// Uma máquina de mentira numa pasta temporária, com espaço e acento no caminho
    /// (como "C:\Users\José da Silva\OneDrive - Empresa").
    fn fake_machine(name: &str) -> (PathBuf, Paths) {
        let root = std::env::temp_dir().join(format!("pm-ia máquina {name} {}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("José da Silva");
        let env = |k: &str| -> Option<std::ffi::OsString> {
            match k {
                "USERPROFILE" => Some(home.clone().into()),
                "APPDATA" => Some(home.join(r"AppData\Roaming").into()),
                "LOCALAPPDATA" => Some(home.join(r"AppData\Local").into()),
                "PATH" => Some(root.join("sem codex").into()),
                _ => None,
            }
        };
        (root.clone(), Paths::from_env(env, |_| None))
    }

    #[test]
    fn maquina_sem_nada_instalado() {
        let (root, paths) = fake_machine("vazia");
        let mut w = Worker::new(paths, false);
        w.active = true;
        let u = w.tick(T);
        assert!(u.claude.limits.is_none() && u.codex.limits.is_none());
        assert!(!u.claude.installed && !u.codex.installed);
        assert!(u.claude.note.as_deref().unwrap().starts_with("Claude Code não encontrado"));
        assert!(u.codex.note.as_deref().unwrap().starts_with("Codex não encontrado"));
        // Sem registros: o consumo existe (lido) e está vazio, sem pânico.
        assert_eq!(u.claude.recent.as_ref().map(|c| c.projects.len()), Some(0));
        assert!(u.codex.recent.is_none());
        // Com a consulta própria ligada e sem login, nada vai à rede.
        w.network = true;
        let u = w.tick(T + 30);
        assert!(u.claude.limits.is_none());
        assert_eq!(w.state.claude.gate.last_attempt, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn maquina_com_arquivos_estragados() {
        let (root, paths) = fake_machine("estragada");
        let claude = paths.claude_dir.clone().unwrap();
        let project = claude.join("projects").join("C--Projetos-Loja Online");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(claude.join(".credentials.json"), "{ corrompido").unwrap();
        std::fs::write(project.join("s1.jsonl"), format!("lixo\n{{\"type\":\"assistant\"\n{LINE}\n\u{0}\u{1}\n")).unwrap();
        let notch = paths.codenotch_dir.clone().unwrap();
        std::fs::create_dir_all(&notch).unwrap();
        std::fs::write(notch.join("usage.json"), "\u{feff}[1, 2").unwrap();
        std::fs::write(notch.join("codex.json"), "").unwrap();
        let bridge = paths.bridge_file.clone().unwrap();
        std::fs::create_dir_all(bridge.parent().unwrap()).unwrap();
        std::fs::write(&bridge, "nulo").unwrap();
        std::fs::create_dir_all(paths.state_file.parent().unwrap()).unwrap();
        std::fs::write(&paths.state_file, "{\"alerts\": \"não é lista\"}").unwrap();
        std::fs::create_dir_all(paths.codex_sessions().unwrap()).unwrap();

        let mut w = Worker::new(paths, true);
        w.active = true;
        let now = super::super::parse_time(&serde_json::json!("2026-11-09T12:00:00Z")).unwrap();
        let u = w.tick(now);
        assert!(u.claude.installed && u.codex.installed);
        assert!(u.claude.limits.is_none());
        // A linha boa no meio do lixo foi lida.
        let week = u.claude.week.unwrap();
        assert_eq!(week.total.requests, 1);
        assert_eq!(week.kinds[0], (Kind::Interactive, week.total));
        // A pasta do projeto do registro não existe nesta máquina de mentira.
        assert_eq!(week.top.len(), 1);
        assert!(!week.top[0].folder_exists);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn statusline_e_codenotch_sem_rede() {
        let (root, paths) = fake_machine("fontes");
        std::fs::create_dir_all(paths.claude_dir.clone().unwrap()).unwrap();
        let bridge = paths.bridge_file.clone().unwrap();
        std::fs::create_dir_all(bridge.parent().unwrap()).unwrap();
        std::fs::write(
            &bridge,
            format!(r#"{{"version":1,"written_at":{},"rate_limits":{{"five_hour":{{"used_percentage":40,"resets_at":{}}}}}}}"#, T * 1000, T + 3600),
        )
        .unwrap();
        let notch = paths.codenotch_dir.clone().unwrap();
        std::fs::create_dir_all(&notch).unwrap();
        std::fs::write(
            notch.join("codex.json"),
            format!(r#"{{"fetched_at":{},"windows":[{{"id":"secondary","used":0.9,"resets_at":{},"duration_ms":604800000}}]}}"#, (T - 60) * 1000, (T + 86_400) * 1000),
        )
        .unwrap();
        let mut w = Worker::new(paths, false);
        let u = w.tick(T + 10);
        let claude = u.claude.limits.unwrap();
        assert_eq!((claude.source, claude.windows[0].used), (Source::StatusLine, 40.0));
        let codex = u.codex.limits.unwrap();
        assert_eq!((codex.source, codex.windows[0].id.as_str()), (Source::Codenotch, SEVEN_DAY));
        // Sem pasta do Codex, mas com leitura do Codenotch: nada de "não encontrado".
        assert!(u.codex.note.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// O que esta máquina devolve, sem rede e sem segredo:
    /// `cargo test limites_ia -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn limites_ia_desta_maquina() {
        let now = super::super::now_secs();
        let paths = Paths::detect();
        let show = |name: &str, l: Option<Limits>| match l {
            Some(l) => {
                let ws: Vec<String> = l.windows.iter().map(|w| format!("{} {:.1}% renova {:?}", w.id, w.used, w.resets_at)).collect();
                println!("{name}: {} (há {} s): {}", l.source.label(), now - l.fetched_at, ws.join(" | "));
            }
            None => println!("{name}: nada"),
        };
        println!(
            "pastas: claude {:?} · codex {:?} · codenotch {:?}",
            paths.claude_dir.as_ref().map(|p| p.is_dir()),
            paths.codex_home.as_ref().map(|p| p.is_dir()),
            paths.codenotch_dir.as_ref().map(|p| p.is_dir())
        );
        let notch = codenotch::read(paths.codenotch_dir.as_deref(), Provider::Claude);
        show("codenotch claude", notch.limits);
        println!("  recuo do codenotch: {:?}", notch.backoff_until.map(|b| b - now));
        show("codenotch codex", codenotch::read(paths.codenotch_dir.as_deref(), Provider::Codex).limits);
        show("statusline", bridge::read(paths.bridge_file.as_deref()));
        let cred = paths.claude_dir.as_deref().and_then(claude_api::read_credential);
        println!("login claude: {:?}", cred.as_ref().map(|c| (c.expires_at.map(|e| e - now), c.plan.clone())));
        let cred = paths.codex_home.as_deref().and_then(codex_api::read_credential);
        println!("login codex: {:?}", cred.map(|c| (c.expires_at.map(|e| e - now), c.plan.clone())));
        println!("codex.exe encontrado: {}", codex_api::find_codex(&paths).is_some());
        let started = Instant::now();
        let mut logs = claude_logs::Scanner::default();
        if let Some(root) = paths.claude_projects() {
            logs.update(&root, now);
        }
        let week = aggregate(logs.records().iter(), now - SEVEN_DAYS_SECS, false);
        println!("claude: {} respostas na semana, lidas em {:?}", week.total.requests, started.elapsed());
        for (kind, t) in &week.kinds {
            println!("  {}: {} respostas, {} tokens novos", kind.label(), t.requests, t.fresh());
        }
        for (model, t) in &week.models {
            println!("  {model}: {:.1}%", super::super::share(t.fresh(), week.total.fresh()));
        }
        let mut codex = codex_logs::Scanner::default();
        if let Some(root) = paths.codex_sessions() {
            codex.update(&root, now);
        }
        println!("codex cli: {} rodadas", codex.records().len());
        show("rollout", codex.limits().cloned());
    }

    #[test]
    fn a_leitura_mais_nova_ganha() {
        assert_eq!(freshest(T, [limits(Source::Codenotch, 10), limits(Source::StatusLine, 20), None]).unwrap().source, Source::StatusLine);
        // Empate: a consulta própria vale mais que o Codenotch.
        assert_eq!(freshest(T, [limits(Source::Codenotch, 10), limits(Source::Api, 10)]).unwrap().source, Source::Api);
        let empty = Some(Limits { windows: vec![], source: Source::Api, fetched_at: 99, plan: None });
        assert_eq!(freshest(T, [empty, limits(Source::CodexLog, 1)]).unwrap().source, Source::CodexLog);
        assert!(freshest(T, [None, None]).is_none());
        // Data no futuro distante: não ganha da leitura de verdade.
        assert_eq!(freshest(T, [limits(Source::Codenotch, i64::MAX), limits(Source::StatusLine, T)]).unwrap().source, Source::StatusLine);
    }

    #[test]
    fn estado_da_rede_sem_token() {
        let net = Net {
            gate: Gate { last_attempt: 5, backoff_until: 6, consecutive_429: 1 },
            reading: limits(Source::Api, 5),
            note: Some("x".into()),
            app_server_after: 7,
        };
        let v = net.to_json();
        let back = Net::from_json(&v);
        assert_eq!((back.gate, back.reading, back.note, back.app_server_after), (net.gate, net.reading.clone(), net.note.clone(), 7));
        assert!(!v.to_string().to_lowercase().contains("token"));
    }

    #[test]
    fn rastro_recomeca_em_janela_nova() {
        let mut t = Trail::default();
        let mut l = Limits {
            windows: vec![Window::new(FIVE_HOUR, 10.0, Some(T + 10_000), Some(FIVE_HOURS_SECS))],
            source: Source::Api,
            fetched_at: T + 1_000,
            plan: None,
        };
        t.record(&mut l, T + 1_000);
        l.fetched_at = T + 2_000;
        l.windows[0].used = 50.0;
        t.record(&mut l, T + 2_000);
        // A mesma leitura de novo não vira ponto.
        t.record(&mut l, T + 2_030);
        assert_eq!(t.points[FIVE_HOUR].len(), 2);
        assert!(matches!(l.windows[0].pace, super::super::Pace::RunsOutAt(_)));
        assert!(l.windows[0].rate.is_some_and(|r| (r - 0.04).abs() < 1e-9));
        // Janela nova (reset 5 h depois): recomeça.
        l.windows[0].resets_at = Some(T + 10_000 + FIVE_HOURS_SECS);
        l.fetched_at = T + 11_000;
        t.record(&mut l, T + 11_000);
        assert_eq!(t.points[FIVE_HOUR].len(), 1);
    }
}
