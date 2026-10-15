//! Processos com problema: janela que não responde, programa que encerra com erro e
//! volta em seguida, e memória que só cresce. Quem avalia é a thread da coleta, a cada
//! rodada (também com a janela na bandeja); as regras ficam em funções sem chamada de
//! sistema, para os testes.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::format;
use crate::platform::{self, HandleGuard};
use crate::sampler::{ProcInfo, ProcKey};

/// Além dos 5 s do Windows: travadas curtas (abrindo um arquivo grande) não contam.
const HUNG_GRACE: Duration = Duration::from_secs(5);
/// Encerramentos com erro em `RESTART_WINDOW` para o programa contar como em loop.
const RESTART_MIN: usize = 3;
const RESTART_WINDOW: Duration = Duration::from_secs(5 * 60);
/// Vida mínima de um encerramento que conta. Ferramentas de linha de comando (`git
/// config --get` sai com 1 quando a chave não existe) vivem menos que isso.
const RESTART_MIN_LIFE_SECS: u64 = 3;
/// A memória é anotada a cada 30 s, e a janela vai até 10 min.
const MEMORY_EVERY: Duration = Duration::from_secs(30);
const MEMORY_SAMPLES: usize = 20;
/// Pelo menos 5 min de pontos antes de concluir qualquer coisa.
const MEMORY_MIN_SAMPLES: usize = 10;
const MEMORY_MIN_GROWTH: u64 = 150 * MB;
/// Processo abrindo cresce por minutos (o VS Code carregando extensões): só depois disso.
const MEMORY_MIN_AGE_SECS: u64 = 120;
const MB: u64 = 1024 * 1024;
/// Ctrl+C no terminal: encerramento pedido pelo usuário, não erro.
const STATUS_CONTROL_C_EXIT: u32 = 0xC000_013A;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Problem {
    /// Janela travada há `secs` segundos (contando os 5 s do Windows).
    NotResponding { secs: u64 },
    /// O mesmo programa (executável e linha de comando) encerrou com erro `exits` vezes
    /// nos últimos 5 minutos, e esta é a cópia que voltou.
    Restarting { exits: usize },
    /// A memória privada cresceu `growth` bytes em `secs` segundos, quase sem parar.
    MemoryGrowing { growth: u64, secs: u64 },
}

impl Problem {
    pub fn label(self) -> &'static str {
        match self {
            Problem::NotResponding { .. } => "não respondendo",
            Problem::Restarting { .. } => "reiniciando",
            Problem::MemoryGrowing { .. } => "memória crescendo",
        }
    }

    pub fn detail(self) -> String {
        match self {
            Problem::NotResponding { secs } => {
                format!("A janela não responde há {}.", format::duration(secs))
            }
            Problem::Restarting { exits } => {
                format!("Encerrou com erro {exits} vezes nos últimos 5 minutos e foi aberto de novo.")
            }
            Problem::MemoryGrowing { growth, secs } => format!(
                "A memória cresceu {} em {}, quase sem parar. Pode ser vazamento.",
                format::bytes(growth),
                format::duration(secs)
            ),
        }
    }

    /// Travado e em loop pedem ação agora; memória crescendo é um aviso.
    pub fn severe(self) -> bool {
        !matches!(self, Problem::MemoryGrowing { .. })
    }
}

/// Programa como o loop de reinício o enxerga: executável, linha de comando e o PID de
/// quem o abre de novo (o supervisor, o nodemon, o gerenciador de serviços). Sem o pai,
/// todos os `conhost.exe 0x4` do sistema seriam o mesmo programa.
type ProgramKey = (String, String, u32);

struct Watched {
    handle: HandleGuard,
    program: ProgramKey,
    start: u64,
}

#[derive(Default)]
pub struct Health {
    hung_since: HashMap<ProcKey, Instant>,
    /// Processos já vistos, com ou sem handle: o `OpenProcess` roda uma vez por processo.
    known: HashSet<ProcKey>,
    /// Handles dos processos que abriram depois do app, para ler o código de saída.
    watched: HashMap<ProcKey, Watched>,
    exits: HashMap<ProgramKey, VecDeque<Instant>>,
    memory: HashMap<ProcKey, VecDeque<u64>>,
    growing: HashMap<ProcKey, (u64, u64)>,
    last_memory: Option<Instant>,
}

impl Health {
    /// Atualiza o estado com a coleta nova e preenche `problems` de cada processo.
    pub fn update(&mut self, now: Instant, processes: &mut [ProcInfo], own_pid: u32) {
        let alive: HashSet<ProcKey> = processes.iter().map(|p| p.key()).collect();
        self.track_exits(now, processes, &alive, own_pid);
        if self.last_memory.is_none_or(|t| now.duration_since(t) >= MEMORY_EVERY) {
            self.last_memory = Some(now);
            self.sample_memory(processes, &alive, own_pid);
        }

        let hung = platform::hung_pids();
        self.hung_since.retain(|k, _| alive.contains(k) && hung.contains(&k.pid));
        for p in processes.iter_mut().filter(|p| p.pid != own_pid) {
            let key = p.key();
            p.problems.clear();
            // Suspenso também trava a janela, mas já tem a marca dele.
            if hung.contains(&p.pid) && !p.suspended {
                let since = *self.hung_since.entry(key).or_insert(now);
                let held = now.duration_since(since);
                if held >= HUNG_GRACE {
                    p.problems.push(Problem::NotResponding { secs: held.as_secs() + 5 });
                }
            }
            if !self.exits.is_empty() {
                let exits = self
                    .exits
                    .iter()
                    .find(|((name, cmd, parent), _)| Some(*parent) == p.parent && *name == p.name && *cmd == p.cmd)
                    .map_or(0, |(_, times)| times.len());
                if exits >= RESTART_MIN {
                    p.problems.push(Problem::Restarting { exits });
                }
            }
            if let Some(&(growth, secs)) = self.growing.get(&key) {
                p.problems.push(Problem::MemoryGrowing { growth, secs });
            }
        }
    }

    /// Lê o código de saída de quem encerrou e abre handle para quem acabou de abrir.
    /// Os que já rodavam quando o app abriu ficam de fora: loop é de quem nasce de novo.
    fn track_exits(&mut self, now: Instant, processes: &[ProcInfo], alive: &HashSet<ProcKey>, own_pid: u32) {
        let now_unix = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let starts: HashMap<u32, u64> = processes.iter().map(|p| (p.pid, p.start_time)).collect();
        let exits = &mut self.exits;
        self.watched.retain(|key, w| {
            let Some(code) = platform::exit_code(&w.handle) else { return alive.contains(key) };
            // Só conta com o pai vivo: quem morre junto com ele (o conhost de um cmd que
            // fechou) não está sendo reaberto por ninguém.
            let parent_alive = starts.get(&w.program.2).is_some_and(|&s| s <= w.start);
            if failed(code) && parent_alive && now_unix.saturating_sub(w.start) >= RESTART_MIN_LIFE_SECS {
                exits.entry(w.program.clone()).or_default().push_back(now);
            }
            false
        });
        for times in exits.values_mut() {
            while times.front().is_some_and(|t| now.duration_since(*t) > RESTART_WINDOW) {
                times.pop_front();
            }
        }
        exits.retain(|_, times| !times.is_empty());

        let first = self.known.is_empty();
        self.known.retain(|k| alive.contains(k));
        for p in processes.iter().filter(|p| p.pid != own_pid) {
            if !self.known.insert(p.key()) || first {
                continue;
            }
            // Sem pai não há quem reabra.
            let Some(parent) = p.parent else { continue };
            if let Some(handle) = platform::watch_exit(p.pid, p.start_time) {
                let program = (p.name.clone(), p.cmd.clone(), parent);
                self.watched.insert(p.key(), Watched { handle, program, start: p.start_time });
            }
        }
    }

    fn sample_memory(&mut self, processes: &[ProcInfo], alive: &HashSet<ProcKey>, own_pid: u32) {
        self.memory.retain(|k, _| alive.contains(k));
        self.growing.clear();
        for p in processes.iter().filter(|p| p.pid != own_pid && p.run_time >= MEMORY_MIN_AGE_SECS) {
            let samples = self.memory.entry(p.key()).or_default();
            if samples.len() == MEMORY_SAMPLES {
                samples.pop_front();
            }
            // Memória privada (a "virtual" do sysinfo): o working set é aparado quando
            // falta RAM e esconderia o crescimento.
            samples.push_back(p.virtual_memory);
            if let Some(growth) = memory_growth(samples.make_contiguous()) {
                let secs = (samples.len() as u64 - 1) * MEMORY_EVERY.as_secs();
                self.growing.insert(p.key(), (growth, secs));
            }
        }
    }
}

/// Código de saída que conta como erro: nem sucesso, nem Ctrl+C.
fn failed(code: u32) -> bool {
    code != 0 && code != STATUS_CONTROL_C_EXIT
}

/// Quanto a memória cresceu, se os pontos (a cada 30 s) mostram crescimento contínuo:
/// pelo menos 150 MB, com 70% dos passos subindo e o último perto do máximo. Um degrau
/// (abriu um arquivo grande e parou) não passa: os passos depois dele são planos.
pub fn memory_growth(samples: &[u64]) -> Option<u64> {
    let (&first, &last) = (samples.first()?, samples.last()?);
    if samples.len() < MEMORY_MIN_SAMPLES {
        return None;
    }
    let growth = last.checked_sub(first)?;
    let max = samples.iter().copied().max().unwrap_or(last);
    let steps = samples.len() - 1;
    let rising = samples.windows(2).filter(|w| w[1] >= w[0] + MB).count();
    (growth >= MEMORY_MIN_GROWTH && rising * 10 >= steps * 7 && last >= max - max / 20).then_some(growth)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memoria_crescendo_sem_parar() {
        // 20 MB a cada 30 s por 5 min: 180 MB.
        let leak: Vec<u64> = (0..10).map(|i| 500 * MB + i * 20 * MB).collect();
        assert_eq!(memory_growth(&leak), Some(180 * MB));
        // Pouco tempo ainda: não conclui.
        assert_eq!(memory_growth(&leak[..9]), None);
        // Cresce, mas pouco.
        let small: Vec<u64> = (0..10).map(|i| 500 * MB + i * 5 * MB).collect();
        assert_eq!(memory_growth(&small), None);
    }

    #[test]
    fn degrau_e_queda_nao_sao_vazamento() {
        // Subiu 400 MB de uma vez e ficou: um passo subindo em nove.
        let step: Vec<u64> = (0..10).map(|i| if i < 2 { 500 * MB } else { 900 * MB }).collect();
        assert_eq!(memory_growth(&step), None);
        // Cresceu e já devolveu boa parte: o último ponto está longe do máximo.
        let mut freed: Vec<u64> = (0..10).map(|i| 500 * MB + i * 40 * MB).collect();
        *freed.last_mut().unwrap() = 700 * MB;
        assert_eq!(memory_growth(&freed), None);
    }

    #[test]
    fn codigos_de_saida() {
        assert!(!failed(0));
        assert!(!failed(STATUS_CONTROL_C_EXIT));
        assert!(failed(1));
        assert!(failed(0xC000_0005)); // violação de acesso
    }

    #[test]
    fn loop_de_reinicio_com_processo_real() {
        // Um `cmd` que vive 4 s e sai com erro, três vezes; a quarta cópia é a que volta.
        let mut health = Health::default();
        let own = std::process::id();
        let mut sys = sysinfo::System::new();
        let collect = |sys: &mut sysinfo::System| -> Vec<ProcInfo> {
            sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
            sys.processes()
                .values()
                .map(|p| {
                    let parent = p.parent().map(|pp| pp.as_u32());
                    let mut info = crate::sampler::tests::proc(p.pid().as_u32(), parent, p.start_time(), &p.name().to_string_lossy());
                    info.cmd = p.cmd().iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" ");
                    info
                })
                .collect()
        };
        let mut procs = collect(&mut sys);
        health.update(Instant::now(), &mut procs, own);
        let spawn = || std::process::Command::new("cmd").args(["/c", "ping -n 5 127.0.0.1 >nul & exit 3"]).spawn().unwrap();
        for _ in 0..3 {
            let mut child = spawn();
            let mut procs = collect(&mut sys);
            health.update(Instant::now(), &mut procs, own);
            assert_eq!(child.wait().unwrap().code(), Some(3));
            let mut procs = collect(&mut sys);
            health.update(Instant::now(), &mut procs, own);
        }
        let mut child = spawn();
        let mut procs = collect(&mut sys);
        health.update(Instant::now(), &mut procs, own);
        let me = procs.iter().find(|p| p.pid == child.id()).expect("cópia nova na coleta");
        assert_eq!(me.problems, vec![Problem::Restarting { exits: 3 }]);
        // Só ela: os `conhost` e os outros `cmd` do sistema não entram.
        let marked = procs.iter().filter(|p| p.problems.iter().any(|x| matches!(x, Problem::Restarting { .. }))).count();
        assert_eq!(marked, 1);
        child.kill().unwrap();
        let _ = child.wait();
    }
}
