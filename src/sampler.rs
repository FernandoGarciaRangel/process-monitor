//! Thread de coleta. Ela é dona do sysinfo e do monitor de GPU, mede tudo a cada
//! intervalo e publica um [`Snapshot`] imutável. A UI só lê o snapshot mais recente,
//! então nunca trava esperando o sistema operacional.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sysinfo::{
    CpuRefreshKind, DiskKind, DiskRefreshKind, Disks, MemoryRefreshKind, Networks, ProcessRefreshKind,
    ProcessesToUpdate, System, UpdateKind,
};

use crate::gpu::GpuMonitor;
use crate::platform;
use crate::ports::{self, Connection, Listener};

/// Pontos guardados em cada série. Com 1 s de intervalo, um minuto de histórico.
pub const HISTORY_LEN: usize = 60;

/// Os valores mais recentes de uma medida, até `cap` pontos; o mais antigo sai primeiro.
#[derive(Clone, Debug)]
pub struct Series {
    values: VecDeque<f32>,
    cap: usize,
}

impl Default for Series {
    fn default() -> Self {
        Self::with_capacity(HISTORY_LEN)
    }
}

impl Series {
    pub fn with_capacity(cap: usize) -> Self {
        Self { values: VecDeque::new(), cap: cap.max(2) }
    }

    /// Quantos pontos cabem: é a largura do gráfico, mesmo com o histórico incompleto.
    pub fn capacity(&self) -> usize {
        self.cap
    }

    pub fn push(&mut self, value: f32) {
        if self.values.len() == self.cap {
            self.values.pop_front();
        }
        self.values.push_back(value);
    }

    pub fn values(&self) -> impl ExactSizeIterator<Item = f32> + '_ {
        self.values.iter().copied()
    }

    pub fn first(&self) -> f32 {
        self.values.front().copied().unwrap_or(0.0)
    }

    pub fn last(&self) -> f32 {
        self.values.back().copied().unwrap_or(0.0)
    }

    pub fn max(&self) -> f32 {
        self.values.iter().copied().fold(0.0, f32::max)
    }

    /// Muda a capacidade guardando os pontos mais recentes.
    pub fn resize(&mut self, cap: usize) {
        self.cap = cap.max(2);
        while self.values.len() > self.cap {
            self.values.pop_front();
        }
    }

    /// Os últimos `n` pontos, como uma série de capacidade `n`.
    pub fn tail(&self, n: usize) -> Series {
        let mut tail = Series::with_capacity(n);
        let skip = self.values.len().saturating_sub(tail.cap);
        tail.values.extend(self.values.iter().skip(skip));
        tail
    }
}

#[derive(Clone, Debug)]
pub struct ProcInfo {
    pub pid: u32,
    pub parent: Option<u32>,
    /// Nome do executável (`claude.exe`). É a identidade usada para agrupar e proteger.
    pub name: String,
    /// Descrição do arquivo (`Claude Code`), quando o executável traz uma.
    pub description: Option<String>,
    pub exe: Option<PathBuf>,
    pub cmd: String,
    pub cwd: Option<PathBuf>,
    pub user: Option<String>,
    /// Segundos Unix. Junto com o PID, identifica o processo sem ambiguidade.
    pub start_time: u64,
    pub run_time: u64,
    /// 0–100, já dividido pelo número de núcleos lógicos.
    pub cpu: f32,
    pub memory: u64,
    pub virtual_memory: u64,
    pub disk_read: f64,
    pub disk_write: f64,
    pub disk_total_read: u64,
    pub disk_total_written: u64,
    pub gpu: f32,
}

impl ProcInfo {
    pub fn key(&self) -> ProcKey {
        ProcKey { pid: self.pid, start: self.start_time }
    }

    pub fn disk_rate(&self) -> f64 {
        self.disk_read + self.disk_write
    }

    /// Nome para exibir: a descrição do arquivo, ou o nome do executável sem ela.
    pub fn display_name(&self) -> &str {
        self.description.as_deref().unwrap_or(&self.name)
    }

    /// O nome do executável só vale a pena mostrar ao lado quando acrescenta algo
    /// (`Python` + `python.exe` não; `Claude Code` + `claude.exe` sim).
    pub fn exe_hint(&self) -> Option<&str> {
        let description = self.description.as_deref()?;
        let stem = self.name.strip_suffix(".exe").or_else(|| self.name.strip_suffix(".EXE")).unwrap_or(&self.name);
        (!description.eq_ignore_ascii_case(stem) && !description.eq_ignore_ascii_case(&self.name)).then_some(&self.name)
    }
}

/// Identidade estável de um processo: o PID sozinho pode ser reaproveitado.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProcKey {
    pub pid: u32,
    pub start: u64,
}

#[derive(Clone, Debug)]
pub struct DiskInfo {
    pub mount: String,
    pub label: String,
    pub file_system: String,
    pub kind: &'static str,
    pub total: u64,
    pub available: u64,
    pub read_rate: f64,
    pub write_rate: f64,
}

#[derive(Clone, Debug)]
pub struct GpuInfo {
    pub name: String,
    pub utilization: f32,
    pub vram_used: u64,
    pub vram_total: u64,
}

#[derive(Clone, Debug)]
pub struct History {
    pub cpu: Series,
    pub cores: Vec<Series>,
    pub memory: Series,
    pub disk_read: Series,
    pub disk_write: Series,
    pub net_rx: Series,
    pub net_tx: Series,
    pub gpus: Vec<Series>,
}

impl History {
    fn new(cap: usize) -> Self {
        let s = || Series::with_capacity(cap);
        Self {
            cpu: s(),
            cores: Vec::new(),
            memory: s(),
            disk_read: s(),
            disk_write: s(),
            net_rx: s(),
            net_tx: s(),
            gpus: Vec::new(),
        }
    }

    /// Capacidade de cada série: quantas coletas cabem na janela de tempo.
    pub fn points(window: Duration, interval: Duration) -> usize {
        (window.as_millis() / interval.as_millis().max(1)) as usize
    }

    fn resize(&mut self, cap: usize) {
        for s in [&mut self.cpu, &mut self.memory, &mut self.disk_read, &mut self.disk_write, &mut self.net_rx, &mut self.net_tx]
        {
            s.resize(cap);
        }
        for s in self.cores.iter_mut().chain(self.gpus.iter_mut()) {
            s.resize(cap);
        }
    }
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    /// Cresce a cada coleta; a UI usa para saber se chegou dado novo.
    pub seq: u64,
    pub host: String,
    pub uptime: u64,

    pub cpu: f32,
    pub cores: Vec<f32>,
    pub cpu_brand: String,
    pub cpu_freq_mhz: u64,
    pub physical_cores: Option<usize>,

    pub mem_total: u64,
    pub mem_used: u64,
    pub mem_available: u64,
    pub swap_total: u64,
    pub swap_used: u64,

    pub disks: Vec<DiskInfo>,
    pub disk_read: f64,
    pub disk_write: f64,

    pub net_rx: f64,
    pub net_tx: f64,
    pub net_rx_total: u64,
    pub net_tx_total: u64,
    pub net_adapters: Vec<String>,

    pub gpus: Vec<GpuInfo>,
    pub processes: Vec<ProcInfo>,
    /// Portas TCP em escuta, lidas no mesmo instante que os processos.
    pub listeners: Vec<Listener>,
    /// As demais conexões TCP, para o painel de detalhes.
    pub connections: Vec<Connection>,
    pub history: History,
    /// Quanto tempo as séries de `history` cobrem.
    pub history_window: Duration,
}

impl Snapshot {
    pub fn mem_percent(&self) -> f32 {
        percent_of(self.mem_used, self.mem_total)
    }

    pub fn find(&self, key: ProcKey) -> Option<&ProcInfo> {
        self.processes.iter().find(|p| p.key() == key)
    }

    /// O pai só conta se nasceu antes do filho; senão o PID pai foi reaproveitado.
    pub fn parent_of(&self, child: &ProcInfo) -> Option<&ProcInfo> {
        let ppid = child.parent?;
        self.processes.iter().find(|p| p.pid == ppid && p.start_time <= child.start_time)
    }

    pub fn children_of(&self, parent: &ProcInfo) -> Vec<&ProcInfo> {
        children_indices(&self.processes, parent).into_iter().map(|i| &self.processes[i]).collect()
    }

    /// Descendentes de `root` com os filhos antes dos pais e `root` por último:
    /// a ordem certa para finalizar uma árvore sem deixar órfãos sendo recriados.
    pub fn tree_postorder(&self, root: ProcKey) -> Vec<&ProcInfo> {
        let Some(idx) = self.processes.iter().position(|p| p.key() == root) else {
            return Vec::new();
        };
        tree_postorder(&self.processes, idx).into_iter().map(|i| &self.processes[i]).collect()
    }
}

fn children_indices(procs: &[ProcInfo], parent: &ProcInfo) -> Vec<usize> {
    procs
        .iter()
        .enumerate()
        .filter(|(_, p)| {
            p.parent == Some(parent.pid) && p.pid != parent.pid && p.start_time >= parent.start_time
        })
        .map(|(i, _)| i)
        .collect()
}

fn tree_postorder(procs: &[ProcInfo], root: usize) -> Vec<usize> {
    fn visit(procs: &[ProcInfo], idx: usize, seen: &mut HashSet<usize>, out: &mut Vec<usize>) {
        if !seen.insert(idx) {
            return;
        }
        for child in children_indices(procs, &procs[idx]) {
            visit(procs, child, seen, out);
        }
        out.push(idx);
    }
    let mut out = Vec::new();
    visit(procs, root, &mut HashSet::new(), &mut out);
    out
}

pub fn percent_of(part: u64, total: u64) -> f32 {
    if total == 0 { 0.0 } else { (part as f64 / total as f64 * 100.0) as f32 }
}

enum Command {
    Refresh,
    SetInterval(Duration),
    SetHistoryWindow(Duration),
}

pub struct Sampler {
    latest: Arc<Mutex<Option<Arc<Snapshot>>>>,
    tx: Sender<Command>,
}

impl Sampler {
    /// `window`: quanto tempo os gráficos de desempenho mostram.
    pub fn spawn(ctx: egui::Context, interval: Duration, window: Duration) -> Self {
        let latest = Arc::new(Mutex::new(None));
        let (tx, rx) = mpsc::channel();
        let shared = latest.clone();
        std::thread::Builder::new()
            .name("sampler".into())
            .spawn(move || run(ctx, shared, rx, interval, window))
            .expect("thread de coleta");
        Self { latest, tx }
    }

    pub fn latest(&self) -> Option<Arc<Snapshot>> {
        self.latest.lock().ok()?.clone()
    }

    /// Coleta já, sem esperar o intervalo (usado depois de finalizar um processo).
    pub fn refresh_now(&self) {
        let _ = self.tx.send(Command::Refresh);
    }

    pub fn set_interval(&self, interval: Duration) {
        let _ = self.tx.send(Command::SetInterval(interval));
    }

    pub fn set_history_window(&self, window: Duration) {
        let _ = self.tx.send(Command::SetHistoryWindow(window));
    }
}

fn process_refresh_kind() -> ProcessRefreshKind {
    // exe, cmd, cwd e usuário não mudam durante a vida do processo e custam caro
    // (cmd e cwd exigem ler a memória do processo): só na primeira vez.
    ProcessRefreshKind::nothing()
        .with_cpu()
        .with_memory()
        .with_disk_usage()
        .with_exe(UpdateKind::OnlyIfNotSet)
        .with_cmd(UpdateKind::OnlyIfNotSet)
        .with_cwd(UpdateKind::OnlyIfNotSet)
        .with_user(UpdateKind::OnlyIfNotSet)
        .without_tasks()
}

fn run(
    ctx: egui::Context,
    latest: Arc<Mutex<Option<Arc<Snapshot>>>>,
    rx: Receiver<Command>,
    mut interval: Duration,
    window: Duration,
) {
    let mut sys = System::new();
    let disk_kind = DiskRefreshKind::nothing().with_storage().with_io_usage();
    let mut disks = Disks::new_with_refreshed_list_specifics(disk_kind.with_kind());
    let mut networks = Networks::new_with_refreshed_list();
    let mut gpu = GpuMonitor::new();
    let mut users: HashMap<String, Option<String>> = HashMap::new();
    // Ler a informação de versão abre o arquivo: uma vez por executável.
    let mut descriptions: HashMap<PathBuf, Option<String>> = HashMap::new();
    let mut window = window;
    let mut history = History::new(History::points(window, interval));

    let host = System::host_name().unwrap_or_default();
    let physical_cores = System::physical_core_count();

    // Base para as taxas: CPU e I/O precisam de duas leituras.
    sys.refresh_cpu_all();
    sys.refresh_memory_specifics(MemoryRefreshKind::everything());
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, process_refresh_kind());
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL.max(Duration::from_millis(250)));

    let mut last = Instant::now();
    let mut seq = 0u64;
    loop {
        let now = Instant::now();
        let dt = now.duration_since(last).as_secs_f64().max(0.05);
        last = now;
        seq += 1;

        sys.refresh_cpu_specifics(CpuRefreshKind::nothing().with_cpu_usage().with_frequency());
        sys.refresh_memory_specifics(MemoryRefreshKind::everything());
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, process_refresh_kind());
        disks.refresh_specifics(true, disk_kind);
        networks.refresh(true);
        let gpu_sample = gpu.as_mut().map(|g| g.sample()).unwrap_or_default();

        let logical = sys.cpus().len().max(1) as f32;
        let cores: Vec<f32> = sys.cpus().iter().map(|c| c.cpu_usage()).collect();
        let cpu = sys.global_cpu_usage();

        let processes: Vec<ProcInfo> = sys
            .processes()
            .values()
            .map(|p| {
                let disk = p.disk_usage();
                let user = p.user_id().and_then(|uid| {
                    let sid = uid.to_string();
                    users.entry(sid).or_insert_with_key(|sid| resolve_user(sid)).clone()
                });
                let description = p.exe().and_then(|exe| {
                    descriptions
                        .entry(exe.to_path_buf())
                        .or_insert_with_key(|path| platform::file_description(path))
                        .clone()
                });
                ProcInfo {
                    pid: p.pid().as_u32(),
                    parent: p.parent().map(|pp| pp.as_u32()),
                    name: p.name().to_string_lossy().into_owned(),
                    description,
                    exe: p.exe().map(|e| e.to_path_buf()),
                    cmd: join_cmd(p.cmd()),
                    cwd: p.cwd().map(|c| c.to_path_buf()),
                    user,
                    start_time: p.start_time(),
                    run_time: p.run_time(),
                    cpu: (p.cpu_usage() / logical).clamp(0.0, 100.0),
                    memory: p.memory(),
                    virtual_memory: p.virtual_memory(),
                    disk_read: disk.read_bytes as f64 / dt,
                    disk_write: disk.written_bytes as f64 / dt,
                    disk_total_read: disk.total_read_bytes,
                    disk_total_written: disk.total_written_bytes,
                    gpu: gpu_sample.per_process.get(&p.pid().as_u32()).copied().unwrap_or(0.0),
                }
            })
            .collect();

        let disk_list: Vec<DiskInfo> = disks
            .list()
            .iter()
            .map(|d| {
                let usage = d.usage();
                DiskInfo {
                    mount: d.mount_point().display().to_string(),
                    label: d.name().to_string_lossy().into_owned(),
                    file_system: d.file_system().to_string_lossy().into_owned(),
                    kind: match d.kind() {
                        DiskKind::SSD => "SSD",
                        DiskKind::HDD => "HDD",
                        _ => "Disco",
                    },
                    total: d.total_space(),
                    available: d.available_space(),
                    read_rate: usage.read_bytes as f64 / dt,
                    write_rate: usage.written_bytes as f64 / dt,
                }
            })
            .collect();
        let disk_read: f64 = disk_list.iter().map(|d| d.read_rate).sum();
        let disk_write: f64 = disk_list.iter().map(|d| d.write_rate).sum();

        let (mut rx_bytes, mut tx_bytes, mut rx_total, mut tx_total) = (0u64, 0u64, 0u64, 0u64);
        let mut active: Vec<(String, u64)> = Vec::new();
        for (name, data) in networks.list() {
            rx_bytes += data.received();
            tx_bytes += data.transmitted();
            rx_total += data.total_received();
            tx_total += data.total_transmitted();
            if data.total_received() + data.total_transmitted() > 0 {
                active.push((name.clone(), data.total_received() + data.total_transmitted()));
            }
        }
        active.sort_by_key(|a| std::cmp::Reverse(a.1));

        let gpus: Vec<GpuInfo> = gpu
            .as_ref()
            .map(|g| {
                g.adapters()
                    .iter()
                    .enumerate()
                    .map(|(i, a)| GpuInfo {
                        name: a.name.clone(),
                        utilization: gpu_sample.utilization.get(i).copied().unwrap_or(0.0),
                        vram_used: gpu_sample.vram_used.get(i).copied().unwrap_or(0),
                        vram_total: a.vram_total,
                    })
                    .collect()
            })
            .unwrap_or_default();

        let mem_total = sys.total_memory();
        let mem_used = sys.used_memory();
        history.cpu.push(cpu);
        let cap = history.cpu.capacity();
        history.cores.resize_with(cores.len(), || Series::with_capacity(cap));
        for (series, value) in history.cores.iter_mut().zip(&cores) {
            series.push(*value);
        }
        history.memory.push(percent_of(mem_used, mem_total));
        history.disk_read.push(disk_read as f32);
        history.disk_write.push(disk_write as f32);
        history.net_rx.push((rx_bytes as f64 / dt) as f32);
        history.net_tx.push((tx_bytes as f64 / dt) as f32);
        history.gpus.resize_with(gpus.len(), || Series::with_capacity(cap));
        for (series, g) in history.gpus.iter_mut().zip(&gpus) {
            series.push(g.utilization);
        }

        let snapshot = Snapshot {
            seq,
            host: host.clone(),
            uptime: System::uptime(),
            cpu,
            cores,
            cpu_brand: sys.cpus().first().map(|c| c.brand().trim().to_owned()).unwrap_or_default(),
            cpu_freq_mhz: sys.cpus().iter().map(|c| c.frequency()).max().unwrap_or(0),
            physical_cores,
            mem_total,
            mem_used,
            mem_available: sys.available_memory(),
            swap_total: sys.total_swap(),
            swap_used: sys.used_swap(),
            disks: disk_list,
            disk_read,
            disk_write,
            net_rx: rx_bytes as f64 / dt,
            net_tx: tx_bytes as f64 / dt,
            net_rx_total: rx_total,
            net_tx_total: tx_total,
            net_adapters: active.into_iter().take(4).map(|(n, _)| n).collect(),
            gpus,
            processes,
            listeners: ports::listeners(),
            connections: ports::connections(),
            history: history.clone(),
            history_window: window,
        };

        if let Ok(mut slot) = latest.lock() {
            *slot = Some(Arc::new(snapshot));
        }
        ctx.request_repaint();

        // Espera o intervalo, mas acorda antes se a UI pedir uma coleta ou mudar o intervalo.
        match rx.recv_timeout(interval) {
            Ok(command) => {
                match command {
                    Command::SetInterval(d) => interval = d,
                    Command::SetHistoryWindow(w) => window = w,
                    Command::Refresh => {}
                }
                // Mesma janela de tempo com outro intervalo = outro número de pontos.
                history.resize(History::points(window, interval));
                // A CPU precisa de um intervalo mínimo entre leituras para não sair zerada.
                let since = last.elapsed();
                if since < sysinfo::MINIMUM_CPU_UPDATE_INTERVAL {
                    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL - since);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn resolve_user(sid: &str) -> Option<String> {
    platform::account_name(sid).or_else(|| match sid {
        "S-1-5-18" => Some("SYSTEM".to_owned()),
        "S-1-5-19" => Some("LOCAL SERVICE".to_owned()),
        "S-1-5-20" => Some("NETWORK SERVICE".to_owned()),
        s if s.starts_with("S-1-5-90-") => Some("DWM".to_owned()),
        s if s.starts_with("S-1-5-96-") => Some("UMFD".to_owned()),
        _ => None,
    })
}

/// Junta os argumentos, com aspas nos que têm espaço, como apareceriam no terminal.
fn join_cmd(args: &[std::ffi::OsString]) -> String {
    args.iter()
        .map(|a| {
            let s = a.to_string_lossy();
            if s.contains(' ') && !s.starts_with('"') { format!("\"{s}\"") } else { s.into_owned() }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn proc(pid: u32, parent: Option<u32>, start: u64, name: &str) -> ProcInfo {
        ProcInfo {
            pid,
            parent,
            name: name.to_owned(),
            description: None,
            exe: None,
            cmd: String::new(),
            cwd: None,
            user: None,
            start_time: start,
            run_time: 0,
            cpu: 0.0,
            memory: 0,
            virtual_memory: 0,
            disk_read: 0.0,
            disk_write: 0.0,
            disk_total_read: 0,
            disk_total_written: 0,
            gpu: 0.0,
        }
    }

    #[test]
    fn arvore_filhos_antes_dos_pais() {
        let procs = vec![
            proc(10, None, 100, "raiz"),
            proc(11, Some(10), 110, "filho a"),
            proc(12, Some(11), 120, "neto"),
            proc(13, Some(10), 130, "filho b"),
            proc(14, Some(99), 140, "outro"),
        ];
        let order: Vec<u32> = tree_postorder(&procs, 0).into_iter().map(|i| procs[i].pid).collect();
        assert_eq!(order.last(), Some(&10));
        let pos = |pid| order.iter().position(|&p| p == pid).unwrap();
        assert!(pos(12) < pos(11));
        assert!(!order.contains(&14));
        assert_eq!(order.len(), 4);
    }

    #[test]
    fn arvore_ignora_pid_reaproveitado() {
        // O processo 21 diz que o pai é 20, mas nasceu antes do 20 atual:
        // o pai verdadeiro morreu e o PID foi reaproveitado.
        let procs = vec![proc(20, None, 500, "novo dono do pid"), proc(21, Some(20), 100, "órfão antigo")];
        let order = tree_postorder(&procs, 0);
        assert_eq!(order, vec![0]);
    }

    #[test]
    fn arvore_sem_laco_infinito() {
        let procs = vec![proc(1, Some(2), 100, "a"), proc(2, Some(1), 100, "b")];
        let order = tree_postorder(&procs, 0);
        assert_eq!(order.len(), 2);
    }

    #[test]
    fn nome_amigavel() {
        let mut p = proc(1, None, 1, "claude.exe");
        assert_eq!(p.display_name(), "claude.exe");
        assert_eq!(p.exe_hint(), None);
        p.description = Some("Claude Code".to_owned());
        assert_eq!(p.display_name(), "Claude Code");
        assert_eq!(p.exe_hint(), Some("claude.exe"));
        // Descrição igual ao nome do arquivo não repete o executável ao lado.
        let mut py = proc(2, None, 1, "python.exe");
        py.description = Some("Python".to_owned());
        assert_eq!(py.exe_hint(), None);
    }

    #[test]
    fn series_limita_tamanho() {
        let mut s = Series::default();
        for i in 0..(HISTORY_LEN + 10) {
            s.push(i as f32);
        }
        assert_eq!(s.values().len(), HISTORY_LEN);
        assert_eq!(s.last(), (HISTORY_LEN + 9) as f32);
        assert_eq!(s.first(), 10.0);

        let mut long = Series::with_capacity(600);
        for i in 0..700 {
            long.push(i as f32);
        }
        assert_eq!(long.values().len(), 600);
        assert_eq!(long.first(), 100.0);
        let tail = long.tail(60);
        assert_eq!((tail.capacity(), tail.values().len(), tail.first(), tail.last()), (60, 60, 640.0, 699.0));
    }

    #[test]
    fn join_cmd_aspas() {
        let args = ["C:\\Program Files\\app.exe".into(), "--flag".into()];
        assert_eq!(join_cmd(&args), "\"C:\\Program Files\\app.exe\" --flag");
    }
}
