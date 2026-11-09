//! Alertas: porta vigiada que caiu ou voltou, processo com CPU alta por um tempo,
//! processo com memória alta, processo travado ou reiniciando em loop, disco quase
//! cheio, problemas de rede e limites de IA. A avaliação roda na thread da coleta (e,
//! para a IA, na thread `usage`), que continuam vivas com a janela escondida na bandeja;
//! aqui fica só a regra, sem chamada de sistema.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::format;
use crate::health::Problem;
use crate::netcheck::Issue;
use crate::ports::Listener;
use crate::sampler::{DiskInfo, ProcInfo, ProcKey};

/// Uma porta vigiada só "caiu" depois de ficar fora esse tempo: reinício rápido não avisa.
const PORT_GRACE: Duration = Duration::from_secs(3);
/// Para avisar de novo, a CPU tem que cair abaixo desta fração do limite.
const CPU_RESET: f32 = 0.8;
/// Idem para a memória.
const MEMORY_RESET: f64 = 0.9;
/// O disco só avisa de novo depois de liberar esses pontos percentuais acima do limite.
const DISK_RESET_POINTS: f32 = 5.0;
/// Um programa em loop avisa no máximo uma vez nesse tempo: cada cópia nova tem outro
/// PID, e sem isso o aviso viria a cada reinício.
const RESTART_QUIET: Duration = Duration::from_secs(10 * 60);
/// O mesmo problema de rede avisa no máximo uma vez nesse tempo: conexão que cai e
/// volta a cada minuto não vira um balão por minuto.
const NETWORK_QUIET: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub watched_ports: Vec<u16>,
    pub ports_enabled: bool,
    pub cpu_enabled: bool,
    /// 0–100, sobre o total da máquina (como a coluna CPU).
    pub cpu_percent: f32,
    pub cpu_secs: u64,
    pub memory_enabled: bool,
    pub memory_bytes: u64,
    /// Não respondendo e reiniciando em loop (`health`).
    pub problems_enabled: bool,
    pub disk_enabled: bool,
    /// Avisa quando o espaço livre de uma unidade cai abaixo desta porcentagem.
    pub disk_free_percent: f32,
    /// Sem internet, DNS fora, conexão instável, Wi-Fi fraco ou caindo (`netcheck`).
    pub network_enabled: bool,
    /// Limite de IA (Claude, Codex) passou de [`USAGE_THRESHOLDS`].
    pub usage_enabled: bool,
    /// Uma janela que chegou a avisar renovou.
    pub usage_reset_enabled: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            watched_ports: Vec::new(),
            ports_enabled: true,
            cpu_enabled: true,
            cpu_percent: 50.0,
            cpu_secs: 30,
            memory_enabled: true,
            memory_bytes: 4 * 1024 * 1024 * 1024,
            problems_enabled: true,
            disk_enabled: true,
            disk_free_percent: 10.0,
            network_enabled: true,
            usage_enabled: true,
            usage_reset_enabled: true,
        }
    }
}

/// O que abrir quando o usuário clica no aviso.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Process(ProcKey),
    Port(u16),
    Disk,
    Network,
    /// Tela "IA".
    Usage,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Alert {
    pub title: String,
    pub body: String,
    pub target: Target,
}

#[derive(Default)]
pub struct Alerts {
    /// Desde quando cada processo está acima do limite de CPU.
    cpu_since: HashMap<ProcKey, Instant>,
    cpu_alerted: HashSet<ProcKey>,
    memory_alerted: HashSet<ProcKey>,
    /// Travados que já avisaram: avisam de novo só depois de voltar a responder.
    hung_alerted: HashSet<ProcKey>,
    /// Programas em loop (executável e linha de comando) e quando avisaram.
    restart_alerted: HashMap<(String, String), Instant>,
    /// Portas vigiadas que já foram vistas abertas, com o nome de quem escutava.
    port_owner: HashMap<u16, String>,
    port_missing_since: HashMap<u16, Instant>,
    port_down: HashSet<u16>,
    /// Unidades (pelo ponto de montagem) que já avisaram e ainda não liberaram espaço.
    disk_alerted: HashSet<String>,
    /// Problemas de rede presentes na última coleta, pelo tipo.
    network_active: HashSet<&'static str>,
    /// Quando cada tipo de problema de rede avisou pela última vez.
    network_alerted: HashMap<&'static str, Instant>,
    /// Desde quando está sem internet, e se a queda avisou (para avisar a volta).
    offline_since: Option<(Instant, bool)>,
}

impl Alerts {
    /// Compara a coleta nova com o estado e devolve os avisos a mostrar agora.
    /// `own_pid` é o do Process Monitor, que não se denuncia.
    pub fn evaluate(
        &mut self,
        now: Instant,
        processes: &[ProcInfo],
        listeners: &[Listener],
        disks: &[DiskInfo],
        config: &Config,
        own_pid: u32,
    ) -> Vec<Alert> {
        let mut out = Vec::new();
        let alive: HashSet<ProcKey> = processes.iter().map(|p| p.key()).collect();
        self.cpu_since.retain(|k, _| alive.contains(k));
        self.cpu_alerted.retain(|k| alive.contains(k));
        self.memory_alerted.retain(|k| alive.contains(k));
        self.hung_alerted.retain(|k| alive.contains(k));
        self.restart_alerted.retain(|_, at| now.duration_since(*at) < RESTART_QUIET);
        // Desligado, ninguém atualiza a contagem: ao religar ela recomeça do zero, sem
        // avisar na hora de um pico que começou com o alerta desligado.
        if !config.cpu_enabled {
            self.cpu_since.clear();
        }

        for p in processes.iter().filter(|p| p.pid != own_pid) {
            let key = p.key();
            if config.cpu_enabled {
                if p.cpu >= config.cpu_percent {
                    let since = *self.cpu_since.entry(key).or_insert(now);
                    let held = now.duration_since(since);
                    if held >= Duration::from_secs(config.cpu_secs) && self.cpu_alerted.insert(key) {
                        out.push(Alert {
                            title: format!("CPU alta: {}", p.display_name()),
                            body: format!(
                                "{} ({}, PID {}) está em {} da CPU há {}.",
                                p.display_name(),
                                p.name,
                                p.pid,
                                format::percent(p.cpu),
                                format::duration(held.as_secs())
                            ),
                            target: Target::Process(key),
                        });
                    }
                } else {
                    self.cpu_since.remove(&key);
                    if p.cpu < config.cpu_percent * CPU_RESET {
                        self.cpu_alerted.remove(&key);
                    }
                }
            }
            if config.memory_enabled {
                if p.memory >= config.memory_bytes {
                    if self.memory_alerted.insert(key) {
                        out.push(Alert {
                            title: format!("Memória alta: {}", p.display_name()),
                            body: format!(
                                "{} ({}, PID {}) está usando {} de memória.",
                                p.display_name(),
                                p.name,
                                p.pid,
                                format::bytes(p.memory)
                            ),
                            target: Target::Process(key),
                        });
                    }
                } else if (p.memory as f64) < config.memory_bytes as f64 * MEMORY_RESET {
                    self.memory_alerted.remove(&key);
                }
            }
            let hung = p.problems.iter().find(|x| matches!(x, Problem::NotResponding { .. }));
            match hung {
                Some(problem) if config.problems_enabled && self.hung_alerted.insert(key) => {
                    out.push(problem_alert(p, *problem, format!("Não respondendo: {}", p.display_name())));
                }
                Some(_) => {}
                None => {
                    self.hung_alerted.remove(&key);
                }
            }
            if config.problems_enabled
                && let Some(problem) = p.problems.iter().find(|x| matches!(x, Problem::Restarting { .. }))
                && let Entry::Vacant(e) = self.restart_alerted.entry((p.name.clone(), p.cmd.clone()))
            {
                e.insert(now);
                out.push(problem_alert(p, *problem, format!("Reiniciando em loop: {}", p.display_name())));
            }
        }

        // Portas que saíram da lista de vigiadas não guardam estado.
        self.port_owner.retain(|port, _| config.watched_ports.contains(port));
        self.port_missing_since.retain(|port, _| config.watched_ports.contains(port));
        self.port_down.retain(|port| config.watched_ports.contains(port));
        for &port in &config.watched_ports {
            let listener = listeners.iter().find(|l| l.port == port);
            match listener {
                Some(l) => {
                    let owner = processes
                        .iter()
                        .find(|p| p.pid == l.pid)
                        .map(|p| p.display_name().to_owned())
                        .unwrap_or_else(|| format!("PID {}", l.pid));
                    self.port_missing_since.remove(&port);
                    if self.port_down.remove(&port) && config.ports_enabled {
                        out.push(Alert {
                            title: format!("Porta {port} voltou"),
                            body: format!("{owner} está escutando a porta {port} de novo."),
                            target: Target::Port(port),
                        });
                    }
                    self.port_owner.insert(port, owner);
                }
                // Só cai o que já foi visto de pé: vigiar uma porta fechada não avisa nada.
                None if self.port_owner.contains_key(&port) && !self.port_down.contains(&port) => {
                    let since = *self.port_missing_since.entry(port).or_insert(now);
                    if now.duration_since(since) >= PORT_GRACE {
                        self.port_down.insert(port);
                        if config.ports_enabled {
                            let owner = &self.port_owner[&port];
                            out.push(Alert {
                                title: format!("Porta {port} caiu"),
                                body: format!("{owner} parou de escutar a porta {port}."),
                                target: Target::Port(port),
                            });
                        }
                    }
                }
                None => {}
            }
        }

        self.disk_alerted.retain(|mount| disks.iter().any(|d| &d.mount == mount));
        if config.disk_enabled {
            for d in disks.iter().filter(|d| d.total > 0) {
                let free = crate::sampler::percent_of(d.available, d.total);
                if free < config.disk_free_percent {
                    if self.disk_alerted.insert(d.mount.clone()) {
                        out.push(Alert {
                            title: format!("Disco quase cheio: {}", d.mount),
                            body: format!(
                                "Restam {} livres de {} ({}).",
                                format::bytes(d.available),
                                format::bytes(d.total),
                                format::percent(free)
                            ),
                            target: Target::Disk,
                        });
                    }
                } else if free >= config.disk_free_percent + DISK_RESET_POINTS {
                    self.disk_alerted.remove(&d.mount);
                }
            }
        } else {
            self.disk_alerted.clear();
        }
        out
    }
}

impl Alerts {
    /// Avisa cada problema de rede quando aparece e, depois de uma queda que avisou,
    /// quando a internet volta. Os dos processos (não conecta, não fecha conexões)
    /// ficam só na tela: um programa insistindo num servidor bloqueado avisaria sem fim.
    pub fn evaluate_network(&mut self, now: Instant, issues: &[Issue], config: &Config) -> Vec<Alert> {
        let mut out = Vec::new();
        if !config.network_enabled {
            self.network_active.clear();
            self.offline_since = None;
            return out;
        }
        let machine: Vec<&Issue> = issues.iter().filter(|i| i.process().is_none()).collect();
        let mut down_alerted = false;
        for issue in &machine {
            let kind = issue.kind();
            if !self.network_active.insert(kind) {
                continue;
            }
            if self.network_alerted.get(kind).is_some_and(|at| now.duration_since(*at) < NETWORK_QUIET) {
                continue;
            }
            self.network_alerted.insert(kind, now);
            down_alerted |= issue.is_down();
            out.push(Alert { title: issue.title(), body: issue.detail(), target: Target::Network });
        }
        self.network_active.retain(|kind| machine.iter().any(|i| i.kind() == *kind));

        let down = machine.iter().any(|i| i.is_down());
        match &mut self.offline_since {
            None if down => self.offline_since = Some((now, down_alerted)),
            // Sem internet ainda, mas pode ter passado de "roteador" para "provedor".
            Some((_, alerted)) if down => *alerted |= down_alerted,
            Some((since, alerted)) => {
                if *alerted {
                    out.push(Alert {
                        title: "A internet voltou".to_owned(),
                        body: format!("Ficou fora por {}.", format::duration(now.duration_since(*since).as_secs())),
                        target: Target::Network,
                    });
                }
                self.offline_since = None;
            }
            None => {}
        }
        out
    }
}

/// Limites de IA que avisam, em % da janela.
pub const USAGE_THRESHOLDS: [f32; 2] = [75.0, 90.0];
/// Dois `resets_at` mais distantes que isso são janelas diferentes; mais perto, é a mesma
/// janela lida por outra fonte (cada uma arredonda de um jeito).
const USAGE_SAME_WINDOW_SECS: i64 = 30 * 60;

/// Uma janela de limite de IA, como o alerta vê.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageWindow {
    /// "Claude", "Codex".
    pub provider: String,
    pub id: String,
    /// "de 5 h", "semanal".
    pub phrase: String,
    pub used: f32,
    /// Segundos Unix.
    pub resets_at: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct UsageMark {
    resets_at: Option<i64>,
    /// Quantos limiares já avisaram nesta janela.
    level: usize,
    phrase: String,
}

/// Avisos de limite de IA: 75% e 90% uma vez por janela e, se ela chegou a avisar, a
/// renovação. Guardado em disco pela thread `usage`: reabrir o app não repete o aviso.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageAlerts {
    marks: HashMap<(String, String), UsageMark>,
}

impl UsageAlerts {
    /// `now` em segundos Unix.
    pub fn evaluate(&mut self, now: i64, windows: &[UsageWindow], config: &Config) -> Vec<Alert> {
        let mut out = Vec::new();
        if !config.usage_enabled {
            self.marks.clear();
            return out;
        }
        // Renovou: pelo relógio, sem esperar uma leitura nova (que pode demorar).
        let mut renewed: Vec<(String, String)> = Vec::new();
        for ((provider, _), mark) in self.marks.iter_mut() {
            if mark.resets_at.is_some_and(|r| r <= now) {
                if mark.level > 0 && config.usage_reset_enabled {
                    renewed.push((provider.clone(), mark.phrase.clone()));
                }
                mark.level = 0;
                mark.resets_at = None;
            }
        }
        renewed.sort();
        for (provider, phrase) in renewed {
            out.push(Alert {
                title: format!("{provider}: o limite {phrase} renovou"),
                body: "A janela recomeçou do zero.".to_owned(),
                target: Target::Usage,
            });
        }
        for w in windows {
            // Leitura de uma janela que já acabou: a % dela não vale mais.
            if w.resets_at.is_some_and(|r| r <= now) {
                continue;
            }
            let mark = self.marks.entry((w.provider.clone(), w.id.clone())).or_default();
            mark.phrase = w.phrase.clone();
            if let (Some(old), Some(new)) = (mark.resets_at, w.resets_at)
                && (new - old).abs() > USAGE_SAME_WINDOW_SECS
            {
                mark.level = 0;
            }
            if w.resets_at.is_some() {
                mark.resets_at = w.resets_at;
            }
            let crossed = USAGE_THRESHOLDS.iter().filter(|t| w.used >= **t).count();
            if crossed > mark.level {
                mark.level = crossed;
                let threshold = USAGE_THRESHOLDS[crossed - 1];
                let reset = match w.resets_at {
                    Some(r) => format!(" Renova em {}, {}.", format::duration((r - now).max(0) as u64), format::when(r, now)),
                    None => String::new(),
                };
                out.push(Alert {
                    title: format!("{}: {}% do limite {}", w.provider, threshold as u32, w.phrase),
                    body: format!("Já usou {} da janela.{reset}", format::percent(w.used)),
                    target: Target::Usage,
                });
            }
        }
        out
    }

    pub fn to_json(&self) -> serde_json::Value {
        let marks: Vec<serde_json::Value> = self
            .marks
            .iter()
            .map(|((provider, id), m)| {
                serde_json::json!({"provider": provider, "id": id, "resets_at": m.resets_at, "level": m.level, "phrase": m.phrase})
            })
            .collect();
        serde_json::Value::Array(marks)
    }

    pub fn from_json(v: &serde_json::Value) -> Self {
        let marks = v
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| {
                let key = (m.get("provider")?.as_str()?.to_owned(), m.get("id")?.as_str()?.to_owned());
                let mark = UsageMark {
                    resets_at: m.get("resets_at").and_then(serde_json::Value::as_i64),
                    level: (m.get("level")?.as_u64()? as usize).min(USAGE_THRESHOLDS.len()),
                    phrase: m.get("phrase").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned(),
                };
                Some((key, mark))
            })
            .collect();
        Self { marks }
    }
}

fn problem_alert(p: &ProcInfo, problem: Problem, title: String) -> Alert {
    Alert {
        title,
        body: format!("{} ({}, PID {}). {}", p.display_name(), p.name, p.pid, problem.detail()),
        target: Target::Process(p.key()),
    }
}

/// Portas vigiadas como o usuário salva: "5173, 8000".
pub fn parse_ports(text: &str) -> Vec<u16> {
    let mut ports: Vec<u16> = text.split(',').filter_map(|p| p.trim().parse().ok()).filter(|p| *p > 0).collect();
    ports.sort_unstable();
    ports.dedup();
    ports
}

pub fn ports_text(ports: &[u16]) -> String {
    ports.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sampler::tests::proc;
    use std::net::{IpAddr, Ipv4Addr};

    const ME: u32 = 1;

    fn cpu(pid: u32, value: f32) -> ProcInfo {
        let mut p = proc(pid, None, 100, "node.exe");
        p.cpu = value;
        p
    }

    fn listener(port: u16, pid: u32) -> Listener {
        Listener { port, pid, addrs: vec![IpAddr::V4(Ipv4Addr::LOCALHOST)] }
    }

    fn only_cpu() -> Config {
        Config { memory_enabled: false, ports_enabled: false, ..Config::default() }
    }

    #[test]
    fn cpu_alta_so_depois_do_tempo() {
        let config = only_cpu();
        let mut alerts = Alerts::default();
        let t0 = Instant::now();
        // Pico curto não avisa.
        assert!(alerts.evaluate(t0, &[cpu(10, 90.0)], &[], &[], &config, ME).is_empty());
        assert!(alerts.evaluate(t0 + Duration::from_secs(5), &[cpu(10, 10.0)], &[], &[], &config, ME).is_empty());
        // Sustentada pelo tempo inteiro avisa uma vez só.
        let t1 = t0 + Duration::from_secs(10);
        assert!(alerts.evaluate(t1, &[cpu(10, 90.0)], &[], &[], &config, ME).is_empty());
        let fired = alerts.evaluate(t1 + Duration::from_secs(30), &[cpu(10, 90.0)], &[], &[], &config, ME);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].target, Target::Process(ProcKey { pid: 10, start: 100 }));
        assert!(alerts.evaluate(t1 + Duration::from_secs(60), &[cpu(10, 90.0)], &[], &[], &config, ME).is_empty());
    }

    #[test]
    fn cpu_histerese_e_o_proprio_app() {
        let config = only_cpu();
        let mut alerts = Alerts::default();
        let t = Instant::now();
        let at = |s: u64| t + Duration::from_secs(s);
        alerts.evaluate(at(0), &[cpu(10, 90.0)], &[], &[], &config, ME);
        assert_eq!(alerts.evaluate(at(30), &[cpu(10, 90.0)], &[], &[], &config, ME).len(), 1);
        // Logo abaixo do limite (45%): ainda não rearma.
        alerts.evaluate(at(31), &[cpu(10, 45.0)], &[], &[], &config, ME);
        alerts.evaluate(at(32), &[cpu(10, 90.0)], &[], &[], &config, ME);
        assert!(alerts.evaluate(at(70), &[cpu(10, 90.0)], &[], &[], &config, ME).is_empty());
        // Bem abaixo (10%): rearma e avisa de novo depois do tempo.
        alerts.evaluate(at(71), &[cpu(10, 10.0)], &[], &[], &config, ME);
        alerts.evaluate(at(72), &[cpu(10, 90.0)], &[], &[], &config, ME);
        assert_eq!(alerts.evaluate(at(102), &[cpu(10, 90.0)], &[], &[], &config, ME).len(), 1);
        // O próprio Process Monitor nunca.
        alerts.evaluate(at(0), &[cpu(ME, 99.0)], &[], &[], &config, ME);
        assert!(alerts.evaluate(at(200), &[cpu(ME, 99.0)], &[], &[], &config, ME).is_empty());
    }

    #[test]
    fn memoria_alta() {
        let config = Config { cpu_enabled: false, ports_enabled: false, memory_bytes: 1000, ..Config::default() };
        let mut alerts = Alerts::default();
        let t = Instant::now();
        let mut p = proc(10, None, 100, "python.exe");
        p.memory = 1500;
        assert_eq!(alerts.evaluate(t, std::slice::from_ref(&p), &[], &[], &config, ME).len(), 1);
        assert!(alerts.evaluate(t, std::slice::from_ref(&p), &[], &[], &config, ME).is_empty());
        p.memory = 950; // abaixo do limite, mas acima de 90%: não rearma
        alerts.evaluate(t, std::slice::from_ref(&p), &[], &[], &config, ME);
        p.memory = 1500;
        assert!(alerts.evaluate(t, std::slice::from_ref(&p), &[], &[], &config, ME).is_empty());
        p.memory = 100;
        alerts.evaluate(t, std::slice::from_ref(&p), &[], &[], &config, ME);
        p.memory = 1500;
        assert_eq!(alerts.evaluate(t, std::slice::from_ref(&p), &[], &[], &config, ME).len(), 1);
    }

    #[test]
    fn travado_e_em_loop_avisam_uma_vez() {
        let config = Config { cpu_enabled: false, memory_enabled: false, ports_enabled: false, ..Config::default() };
        let mut alerts = Alerts::default();
        let t = Instant::now();
        let at = |s: u64| t + Duration::from_secs(s);
        let mut app = proc(10, Some(1), 100, "app.exe");
        app.problems = vec![Problem::NotResponding { secs: 12 }];
        let fired = alerts.evaluate(at(0), std::slice::from_ref(&app), &[], &[], &config, ME);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].title, "Não respondendo: app.exe");
        assert!(alerts.evaluate(at(1), std::slice::from_ref(&app), &[], &[], &config, ME).is_empty());
        // Voltou a responder e travou de novo: avisa de novo.
        app.problems.clear();
        alerts.evaluate(at(2), std::slice::from_ref(&app), &[], &[], &config, ME);
        app.problems = vec![Problem::NotResponding { secs: 10 }];
        assert_eq!(alerts.evaluate(at(3), std::slice::from_ref(&app), &[], &[], &config, ME).len(), 1);

        // Em loop: cada cópia tem outro PID, mas o programa avisa uma vez em 10 min.
        let copy = |pid: u32| {
            let mut p = proc(pid, Some(1), 100 + u64::from(pid), "node.exe");
            p.cmd = "node server.js".to_owned();
            p.problems = vec![Problem::Restarting { exits: 3 }];
            p
        };
        assert_eq!(alerts.evaluate(at(10), &[copy(20)], &[], &[], &config, ME).len(), 1);
        assert!(alerts.evaluate(at(20), &[copy(21)], &[], &[], &config, ME).is_empty());
        assert_eq!(alerts.evaluate(at(10 + 601), &[copy(22)], &[], &[], &config, ME).len(), 1);

        // Desligado, nada.
        let off = Config { problems_enabled: false, ..config };
        assert!(Alerts::default().evaluate(at(0), &[copy(30)], &[], &[], &off, ME).is_empty());
    }

    #[test]
    fn porta_vigiada_cai_e_volta() {
        let config = Config { cpu_enabled: false, memory_enabled: false, watched_ports: vec![5173], ..Config::default() };
        let mut alerts = Alerts::default();
        let t = Instant::now();
        let at = |ms: u64| t + Duration::from_millis(ms);
        let vite = proc(20, None, 100, "node.exe");
        let procs = std::slice::from_ref(&vite);
        let up = [listener(5173, 20)];
        // Vigiada, mas nunca vista de pé: nada.
        assert!(alerts.evaluate(at(0), procs, &[], &[], &config, ME).is_empty());
        assert!(alerts.evaluate(at(10_000), procs, &[], &[], &config, ME).is_empty());
        // Sobe; some por 1 s (reinício rápido) e volta: nada.
        assert!(alerts.evaluate(at(11_000), procs, &up, &[], &config, ME).is_empty());
        assert!(alerts.evaluate(at(12_000), procs, &[], &[], &config, ME).is_empty());
        assert!(alerts.evaluate(at(13_000), procs, &up, &[], &config, ME).is_empty());
        // Some por mais de 3 s: "caiu", uma vez.
        assert!(alerts.evaluate(at(14_000), procs, &[], &[], &config, ME).is_empty());
        let down = alerts.evaluate(at(17_500), procs, &[], &[], &config, ME);
        assert_eq!(down.len(), 1);
        assert_eq!(down[0].title, "Porta 5173 caiu");
        assert!(alerts.evaluate(at(30_000), procs, &[], &[], &config, ME).is_empty());
        // Volta: "voltou", uma vez.
        let back = alerts.evaluate(at(31_000), procs, &up, &[], &config, ME);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].target, Target::Port(5173));
        assert!(alerts.evaluate(at(32_000), procs, &up, &[], &config, ME).is_empty());
    }

    #[test]
    fn disco_quase_cheio() {
        let config = Config { cpu_enabled: false, memory_enabled: false, ..Config::default() };
        let mut alerts = Alerts::default();
        let t = Instant::now();
        let disk = |available: u64| DiskInfo {
            mount: "C:\\".to_owned(),
            label: String::new(),
            file_system: "NTFS".to_owned(),
            kind: "SSD",
            total: 1000,
            available,
            read_rate: 0.0,
            write_rate: 0.0,
        };
        assert!(alerts.evaluate(t, &[], &[], &[disk(500)], &config, ME).is_empty());
        let fired = alerts.evaluate(t, &[], &[], &[disk(50)], &config, ME);
        assert_eq!((fired.len(), fired[0].target), (1, Target::Disk));
        assert!(alerts.evaluate(t, &[], &[], &[disk(40)], &config, ME).is_empty());
        // Liberar um pouco (12%) não rearma; passar de 15% rearma.
        alerts.evaluate(t, &[], &[], &[disk(120)], &config, ME);
        assert!(alerts.evaluate(t, &[], &[], &[disk(50)], &config, ME).is_empty());
        alerts.evaluate(t, &[], &[], &[disk(200)], &config, ME);
        assert_eq!(alerts.evaluate(t, &[], &[], &[disk(50)], &config, ME).len(), 1);
    }

    #[test]
    fn rede_cai_e_volta() {
        let config = Config::default();
        let mut alerts = Alerts::default();
        let t = Instant::now();
        let at = |s: u64| t + Duration::from_secs(s);
        let down = [Issue::NoInternet { router_ok: true }];
        let fired = alerts.evaluate_network(at(0), &down, &config);
        assert_eq!((fired.len(), fired[0].target), (1, Target::Network));
        // Continua fora: não repete.
        assert!(alerts.evaluate_network(at(5), &down, &config).is_empty());
        // Voltou: avisa com o tempo fora.
        let back = alerts.evaluate_network(at(65), &[], &config);
        assert_eq!(back[0].title, "A internet voltou");
        assert_eq!(back[0].body, format!("Ficou fora por {}.", format::duration(65)));
        // Caiu de novo logo depois: dentro dos 10 min, nem a queda nem a volta avisam.
        assert!(alerts.evaluate_network(at(70), &down, &config).is_empty());
        assert!(alerts.evaluate_network(at(80), &[], &config).is_empty());
        // Depois dos 10 min, avisa de novo.
        assert_eq!(alerts.evaluate_network(at(700), &down, &config).len(), 1);

        // Problema de um processo só aparece na tela.
        let key = ProcKey { pid: 10, start: 100 };
        let stuck = [Issue::SocketLeak { key, process: "app.exe".into(), count: 30 }];
        assert!(Alerts::default().evaluate_network(at(0), &stuck, &config).is_empty());
        // Desligado, nada.
        let off = Config { network_enabled: false, ..Config::default() };
        assert!(Alerts::default().evaluate_network(at(0), &down, &off).is_empty());
    }

    fn usage(id: &str, used: f32, resets_at: Option<i64>) -> UsageWindow {
        UsageWindow { provider: "Claude".into(), id: id.into(), phrase: "de 5 h".into(), used, resets_at }
    }

    #[test]
    fn limite_de_ia_avisa_uma_vez_por_janela() {
        let config = Config::default();
        let mut a = UsageAlerts::default();
        let t = 1_000_000;
        let reset = Some(t + 3600);
        assert!(a.evaluate(t, &[usage("five_hour", 50.0, reset)], &config).is_empty());
        let fired = a.evaluate(t + 10, &[usage("five_hour", 76.0, reset)], &config);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].title, "Claude: 75% do limite de 5 h");
        assert_eq!(fired[0].target, Target::Usage);
        assert!(fired[0].body.starts_with("Já usou 76,0% da janela. Renova em 59 min 50 s"), "{}", fired[0].body);
        assert!(a.evaluate(t + 20, &[usage("five_hour", 80.0, reset)], &config).is_empty());
        // Outra fonte lê a mesma janela com o reset uns segundos diferente: não repete.
        assert!(a.evaluate(t + 30, &[usage("five_hour", 81.0, Some(t + 3605))], &config).is_empty());
        // Pulou de 81 para 95: só o aviso de 90.
        let fired = a.evaluate(t + 40, &[usage("five_hour", 95.0, reset)], &config);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].title, "Claude: 90% do limite de 5 h");
        // Reabrir o app não repete.
        let mut again = UsageAlerts::from_json(&a.to_json());
        assert_eq!(again, a);
        assert!(again.evaluate(t + 50, &[usage("five_hour", 95.0, reset)], &config).is_empty());
        // Renovou (pelo relógio, sem leitura nova); a leitura velha não avisa de novo.
        let fired = a.evaluate(t + 3601, &[usage("five_hour", 95.0, reset)], &config);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].title, "Claude: o limite de 5 h renovou");
        assert!(a.evaluate(t + 3700, &[usage("five_hour", 95.0, reset)], &config).is_empty());
        // Janela nova chegando a 75% avisa de novo.
        let next = Some(t + 3600 + 5 * 3600);
        assert!(a.evaluate(t + 3800, &[usage("five_hour", 10.0, next)], &config).is_empty());
        assert_eq!(a.evaluate(t + 3900, &[usage("five_hour", 75.0, next)], &config).len(), 1);
    }

    #[test]
    fn limite_de_ia_sem_aviso_de_renovacao_a_toa() {
        let mut a = UsageAlerts::default();
        let t = 1_000_000;
        // A janela renovou sem ter avisado nada: não avisa a renovação.
        a.evaluate(t, &[usage("five_hour", 30.0, Some(t + 60))], &Config::default());
        assert!(a.evaluate(t + 61, &[], &Config::default()).is_empty());
        // Renovação desligada: só os limiares.
        let config = Config { usage_reset_enabled: false, ..Config::default() };
        assert_eq!(a.evaluate(t + 100, &[usage("five_hour", 91.0, Some(t + 200))], &config).len(), 1);
        assert!(a.evaluate(t + 201, &[], &config).is_empty());
        // Desligado: nada.
        let off = Config { usage_enabled: false, ..Config::default() };
        assert!(UsageAlerts::default().evaluate(t, &[usage("five_hour", 99.0, None)], &off).is_empty());
    }

    #[test]
    fn portas_como_texto() {
        assert_eq!(parse_ports("8000, 5173,5173, x, 0"), vec![5173, 8000]);
        assert_eq!(ports_text(&[5173, 8000]), "5173, 8000");
    }
}
