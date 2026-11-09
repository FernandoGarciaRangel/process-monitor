//! A thread `usage`: a cada [`TICK`] lê as fontes locais, decide se vai à rede,
//! escolhe a leitura mais nova de cada provedor, atualiza o ritmo e avalia os alertas.
//! Fica viva com a janela na bandeja, porque os alertas dependem dela. O consumo por
//! projeto (os `.jsonl`) só é lido com a tela "IA" aberta.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::gate::Gate;
use super::{
    FIVE_HOUR, FIVE_HOURS_SECS, Limits, Provider, ProviderState, SEVEN_DAY, SEVEN_DAYS_SECS, Source, Usage, aggregate, bridge,
    claude_api, claude_logs, codenotch, codex_api, codex_logs,
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

fn state_path() -> PathBuf {
    super::data_dir().join("usage-state.json")
}

struct State {
    claude: Net,
    codex: Net,
    alerts: UsageAlerts,
}

impl State {
    fn load() -> Self {
        let v = super::read_json(&state_path()).unwrap_or(Value::Null);
        Self {
            claude: v.get("claude").map(Net::from_json).unwrap_or_default(),
            codex: v.get("codex").map(Net::from_json).unwrap_or_default(),
            alerts: v.get("alerts").map(UsageAlerts::from_json).unwrap_or_default(),
        }
    }

    fn save(&self) {
        let v = serde_json::json!({
            "claude": self.claude.to_json(), "codex": self.codex.to_json(), "alerts": self.alerts.to_json(),
        });
        if let Err(e) = super::write_atomic(&state_path(), &v.to_string())
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
            while points.front().is_some_and(|(t, _)| now - t > PACE_SECS) {
                points.pop_front();
            }
            let pts: Vec<(i64, f32)> = points.iter().copied().collect();
            w.pace = super::forecast(w, &pts, now);
            self.series.entry(w.id.clone()).or_insert_with(|| Series::with_capacity(HISTORY_POINTS)).push(w.used);
        }
    }

    fn history(&self) -> Vec<(String, Series)> {
        let mut out: Vec<(String, Series)> = self.series.iter().map(|(k, s)| (k.clone(), s.clone())).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

/// A leitura mais nova entre as que têm janelas.
fn freshest(candidates: impl IntoIterator<Item = Option<Limits>>) -> Option<Limits> {
    candidates.into_iter().flatten().filter(|l| !l.windows.is_empty()).max_by_key(|l| (l.fetched_at, rank(l.source)))
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

fn run(ctx: egui::Context, latest: Arc<Mutex<Arc<Usage>>>, rx: Receiver<Msg>, mut network: bool) {
    let mut state = State::load();
    let mut alert_config = alerts::Config::default();
    let mut sink: Option<AlertSink> = None;
    let mut active = false;
    let mut claude_logs = claude_logs::Scanner::default();
    let mut codex_logs = codex_logs::Scanner::default();
    let mut scanned = false;
    let mut trails: HashMap<Provider, Trail> = HashMap::new();
    let claude_root = claude_logs::root();
    let codex_root = codex_logs::root();
    // Espera um pouco antes da primeira rodada: a abertura é da coleta.
    let mut deadline = Instant::now() + Duration::from_secs(2);

    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(msg) => {
                let collect_now = matches!(msg, Msg::Refresh | Msg::Network(_) | Msg::Active(true));
                match msg {
                    Msg::Active(on) => active = on,
                    Msg::Network(on) => network = on,
                    Msg::Refresh => {}
                    Msg::Alerts(c) => alert_config = c,
                    Msg::AlertSink(s) => sink = Some(s),
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

        // Codex CLI: barato (só as pastas dos últimos dias) e traz a % de cada rodada.
        let codex_has_logs = codex_root.as_ref().is_some_and(|r| r.is_dir());
        if let Some(root) = codex_root.as_ref().filter(|_| codex_has_logs) {
            codex_logs.update(root, now);
        }
        let claude_has_logs = claude_root.as_ref().is_some_and(|r| r.is_dir());
        if active && let Some(root) = claude_root.as_ref().filter(|_| claude_has_logs) {
            claude_logs.update(root, now);
            scanned = true;
        }

        // Claude.
        let notch = codenotch::read(Provider::Claude);
        let line = bridge::read();
        let mut claude_note = None;
        let busy = notch.holds_network(now) || line.as_ref().is_some_and(|l| now - l.fetched_at < bridge::FRESH_SECS);
        if network && !busy && state.claude.gate.may_fetch(now, &state.claude.resets()) {
            fetch_claude(&mut state.claude, now);
            state.save();
        }
        if network {
            claude_note = state.claude.note.clone().filter(|_| !busy);
        }
        let mut claude = freshest([notch.limits, line, network.then(|| state.claude.reading.clone()).flatten()]);

        // Codex.
        let notch = codenotch::read(Provider::Codex);
        let rollout = codex_logs.limits().cloned();
        let mut codex_note = None;
        let busy = notch.holds_network(now);
        if network && !busy && state.codex.gate.may_fetch(now, &state.codex.resets()) {
            fetch_codex(&mut state.codex, now);
            state.save();
        }
        if network {
            codex_note = state.codex.note.clone().filter(|_| !busy);
        }
        let mut codex = freshest([notch.limits, rollout, network.then(|| state.codex.reading.clone()).flatten()]);

        if claude.is_none() && claude_note.is_none() {
            claude_note = Some(no_source_note(Provider::Claude, network));
        }
        if codex.is_none() && codex_note.is_none() {
            codex_note = Some(no_source_note(Provider::Codex, network));
        }

        for (provider, limits) in [(Provider::Claude, &mut claude), (Provider::Codex, &mut codex)] {
            if let Some(l) = limits {
                trails.entry(provider).or_default().record(l, now);
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
        if let Some(sink) = &sink {
            let before = state.alerts.clone();
            for alert in state.alerts.evaluate(now, &windows, &alert_config) {
                sink(&alert);
            }
            if state.alerts != before {
                state.save();
            }
        }

        let consumption = |records: &[super::Record], limits: &Option<Limits>| {
            let start = |id: &str, span: i64| match limits.as_ref().and_then(|l| l.window(id)).filter(|w| !w.expired(now)) {
                Some(w) if w.starts_at().is_some() => (w.starts_at().unwrap_or(now - span), true),
                _ => (now - span, false),
            };
            let (recent, aligned_recent) = start(FIVE_HOUR, FIVE_HOURS_SECS);
            let (week, aligned_week) = start(SEVEN_DAY, SEVEN_DAYS_SECS);
            (aggregate(records.iter(), recent, aligned_recent), aggregate(records.iter(), week, aligned_week))
        };
        let (claude_recent, claude_week) = if scanned { consumption(claude_logs.records(), &claude) } else { Default::default() };
        let (codex_recent, codex_week) = consumption(codex_logs.records(), &codex);

        let usage = Usage {
            claude: ProviderState {
                limits: claude,
                note: claude_note,
                history: trails.get(&Provider::Claude).map(Trail::history).unwrap_or_default(),
                recent: scanned.then_some(claude_recent),
                week: scanned.then_some(claude_week),
                has_logs: claude_has_logs,
            },
            codex: ProviderState {
                limits: codex,
                note: codex_note,
                history: trails.get(&Provider::Codex).map(Trail::history).unwrap_or_default(),
                recent: codex_has_logs.then_some(codex_recent),
                week: codex_has_logs.then_some(codex_week),
                has_logs: codex_has_logs,
            },
            network,
            scanned,
        };
        if let Ok(mut slot) = latest.lock() {
            *slot = Arc::new(usage);
        }
        ctx.request_repaint();
    }
}

fn no_source_note(p: Provider, network: bool) -> String {
    match (p, network) {
        (Provider::Claude, false) => {
            "Sem leitura ainda. A statusline do Claude Code grava a % a cada resposta; com o Codenotch aberto, ele também serve. \
             A consulta própria fica nas configurações."
                .to_owned()
        }
        (Provider::Codex, false) => {
            "Sem leitura ainda. Com o Codenotch aberto ou usando o Codex CLI, a % aparece aqui. A consulta própria fica nas \
             configurações."
                .to_owned()
        }
        (_, true) => "Esperando a primeira leitura.".to_owned(),
    }
}

/// Uma tentativa no servidor do Claude. O token nunca sai de `claude_api`.
fn fetch_claude(net: &mut Net, now: i64) {
    use claude_api::Error;
    // Sem credencial ou com ela vencida, nada vai à rede: não conta como tentativa.
    match claude_api::fetch(now) {
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
fn fetch_codex(net: &mut Net, now: i64) {
    use codex_api::Error;
    let direct = codex_api::fetch(now);
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
                match codex_api::read_app_server(now) {
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
    use crate::usage::Window;

    fn limits(source: Source, at: i64) -> Option<Limits> {
        Some(Limits { windows: vec![Window::new(FIVE_HOUR, 10.0, None, None)], source, fetched_at: at, plan: None })
    }

    /// O que esta máquina devolve, sem rede e sem segredo:
    /// `cargo test limites_ia -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn limites_ia_desta_maquina() {
        let now = super::super::now_secs();
        let show = |name: &str, l: Option<Limits>| match l {
            Some(l) => {
                let ws: Vec<String> = l.windows.iter().map(|w| format!("{} {:.1}% renova {:?}", w.id, w.used, w.resets_at)).collect();
                println!("{name}: {} (há {} s): {}", l.source.label(), now - l.fetched_at, ws.join(" | "));
            }
            None => println!("{name}: nada"),
        };
        let notch = codenotch::read(Provider::Claude);
        if let Some(v) = std::env::var_os("APPDATA").and_then(|d| super::super::read_json(&PathBuf::from(d).join(r"codenotch\usage.json")))
        {
            let ids: Vec<String> = v["windows"].as_array().into_iter().flatten().map(|w| format!("{}={}", w["id"], w["used"])).collect();
            println!("usage.json: status {} · nota {} · fetched_at {} · janelas {}", v["status"], v["note"], v["fetched_at"], ids.join(", "));
        }
        show("codenotch claude", notch.limits);
        println!("  recuo do codenotch: {:?}", notch.backoff_until.map(|b| b - now));
        show("codenotch codex", codenotch::read(Provider::Codex).limits);
        show("statusline", bridge::read());
        let cred = claude_api::read_credential();
        println!("login claude: {:?}", cred.as_ref().map(|c| (c.expires_at.map(|e| e - now), c.plan.clone())));
        println!("login codex: {:?}", codex_api::read_credential().map(|c| (c.expires_at.map(|e| e - now), c.plan.clone())));
        println!("codex.exe: {:?}", codex_api::find_codex());
        let started = Instant::now();
        let mut logs = claude_logs::Scanner::default();
        if let Some(root) = claude_logs::root() {
            logs.update(&root, now);
        }
        let week = aggregate(logs.records().iter(), now - SEVEN_DAYS_SECS, false);
        println!("claude: {} respostas na semana, lidas em {:?}", week.total.requests, started.elapsed());
        for p in week.projects.iter().take(5) {
            println!("  {}: {} respostas, {} tokens novos", p.name(), p.tokens.requests, p.tokens.fresh());
        }
        let mut codex = codex_logs::Scanner::default();
        if let Some(root) = codex_logs::root() {
            codex.update(&root, now);
        }
        println!("codex cli: {} rodadas", codex.records().len());
        show("rollout", codex.limits().cloned());
    }

    #[test]
    fn a_leitura_mais_nova_ganha() {
        assert_eq!(freshest([limits(Source::Codenotch, 10), limits(Source::StatusLine, 20), None]).unwrap().source, Source::StatusLine);
        // Empate: a consulta própria vale mais que o Codenotch.
        assert_eq!(freshest([limits(Source::Codenotch, 10), limits(Source::Api, 10)]).unwrap().source, Source::Api);
        let empty = Some(Limits { windows: vec![], source: Source::Api, fetched_at: 99, plan: None });
        assert_eq!(freshest([empty, limits(Source::CodexLog, 1)]).unwrap().source, Source::CodexLog);
        assert!(freshest([None, None]).is_none());
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
            windows: vec![Window::new(FIVE_HOUR, 10.0, Some(10_000), Some(FIVE_HOURS_SECS))],
            source: Source::Api,
            fetched_at: 1_000,
            plan: None,
        };
        t.record(&mut l, 1_000);
        l.fetched_at = 2_000;
        l.windows[0].used = 50.0;
        t.record(&mut l, 2_000);
        // A mesma leitura de novo não vira ponto.
        t.record(&mut l, 2_030);
        assert_eq!(t.points[FIVE_HOUR].len(), 2);
        assert!(matches!(l.windows[0].pace, super::super::Pace::RunsOutAt(_)));
        // Janela nova (reset 5 h depois): recomeça.
        l.windows[0].resets_at = Some(10_000 + FIVE_HOURS_SECS);
        l.fetched_at = 11_000;
        t.record(&mut l, 11_000);
        assert_eq!(t.points[FIVE_HOUR].len(), 1);
    }
}
