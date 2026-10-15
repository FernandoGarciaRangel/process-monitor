//! Ajustes num processo vivo: suspender e retomar, prioridade, núcleos (afinidade) e
//! modo eficiência. Toda chamada confere a hora de criação, como `platform::kill`: o
//! PID sozinho pode já ser de outro processo.

use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, HANDLE};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Threading::{
    ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, GetPriorityClass, GetProcessAffinityMask,
    GetProcessInformation, HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS, IsProcessCritical, NORMAL_PRIORITY_CLASS,
    OpenProcess, PROCESS_ACCESS_RIGHTS, PROCESS_CREATION_FLAGS, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
    PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SET_INFORMATION, PROCESS_SUSPEND_RESUME, ProcessPowerThrottling, SetPriorityClass, SetProcessAffinityMask,
    SetProcessInformation,
};
use windows::core::{BOOL, s, w};

use crate::platform::{self, HandleGuard, KillError};
use crate::sampler::ProcKey;

/// Suspensos, a tela inteira ou a área de trabalho congelam, e não há como clicar em
/// "Retomar". Os críticos de `platform` também ficam de fora.
const NO_SUSPEND: &[&str] = &["dwm.exe", "explorer.exe", "fontdrvhost.exe", "sihost.exe", "ctfmon.exe"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    Idle,
    BelowNormal,
    Normal,
    AboveNormal,
    High,
}

impl Priority {
    /// Tempo real fica de fora: um processo preso nela trava o mouse e o teclado.
    pub const ALL: [Priority; 5] =
        [Priority::Idle, Priority::BelowNormal, Priority::Normal, Priority::AboveNormal, Priority::High];

    pub fn label(self) -> &'static str {
        match self {
            Priority::Idle => "Baixa",
            Priority::BelowNormal => "Abaixo do normal",
            Priority::Normal => "Normal",
            Priority::AboveNormal => "Acima do normal",
            Priority::High => "Alta",
        }
    }

    fn class(self) -> PROCESS_CREATION_FLAGS {
        match self {
            Priority::Idle => IDLE_PRIORITY_CLASS,
            Priority::BelowNormal => BELOW_NORMAL_PRIORITY_CLASS,
            Priority::Normal => NORMAL_PRIORITY_CLASS,
            Priority::AboveNormal => ABOVE_NORMAL_PRIORITY_CLASS,
            Priority::High => HIGH_PRIORITY_CLASS,
        }
    }

    fn from_class(class: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.class().0 == class)
    }
}

/// O que se conseguiu ler do processo. `None` = sem acesso.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tuning {
    pub priority: Option<Priority>,
    /// (núcleos do processo, núcleos da máquina), como máscaras de bits.
    pub affinity: Option<(u64, u64)>,
    pub efficiency: Option<bool>,
}

/// Lê prioridade, afinidade e modo eficiência. Um handle só, aberto uma vez por seleção.
pub fn read(key: ProcKey) -> Tuning {
    let Ok(guard) = open(key, PROCESS_QUERY_LIMITED_INFORMATION) else { return Tuning::default() };
    unsafe {
        let class = GetPriorityClass(guard.0);
        let priority = if class == 0 { None } else { Priority::from_class(class) };
        let (mut process, mut system) = (0usize, 0usize);
        let affinity = GetProcessAffinityMask(guard.0, &mut process, &mut system)
            .ok()
            .map(|()| (process as u64, system as u64));
        let mut state = PROCESS_POWER_THROTTLING_STATE { Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION, ..Default::default() };
        let efficiency = GetProcessInformation(
            guard.0,
            ProcessPowerThrottling,
            &mut state as *mut _ as *mut _,
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
        .ok()
        .map(|()| {
            state.ControlMask & PROCESS_POWER_THROTTLING_EXECUTION_SPEED != 0
                && state.StateMask & PROCESS_POWER_THROTTLING_EXECUTION_SPEED != 0
        });
        Tuning { priority, affinity, efficiency }
    }
}

pub fn set_priority(key: ProcKey, name: &str, priority: Priority) -> Result<(), KillError> {
    let guard = open_checked(key, name, PROCESS_SET_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION)?;
    unsafe { SetPriorityClass(guard.0, priority.class()).map_err(call_error) }
}

pub fn set_affinity(key: ProcKey, name: &str, mask: u64) -> Result<(), KillError> {
    if mask == 0 {
        return Err(KillError::Other("escolha pelo menos um núcleo".to_owned()));
    }
    let guard = open_checked(key, name, PROCESS_SET_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION)?;
    unsafe { SetProcessAffinityMask(guard.0, mask as usize).map_err(call_error) }
}

/// Modo eficiência como o Gerenciador de Tarefas: EcoQoS e prioridade baixa. Ao
/// desligar, a prioridade volta ao normal.
pub fn set_efficiency(key: ProcKey, name: &str, on: bool) -> Result<(), KillError> {
    let guard = open_checked(key, name, PROCESS_SET_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION)?;
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: if on { PROCESS_POWER_THROTTLING_EXECUTION_SPEED } else { 0 },
    };
    unsafe {
        SetProcessInformation(
            guard.0,
            ProcessPowerThrottling,
            &state as *const _ as *const _,
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
        .map_err(call_error)?;
        let priority = if on { Priority::Idle } else { Priority::Normal };
        SetPriorityClass(guard.0, priority.class()).map_err(call_error)
    }
}

/// Se dá para suspender sem travar a sessão (o motivo, quando não dá).
pub fn suspend_blocked(pid: u32, name: &str) -> Option<&'static str> {
    if platform::is_protected_by_name(pid, name) {
        return Some("Processo crítico do sistema.");
    }
    NO_SUSPEND
        .iter()
        .any(|n| n.eq_ignore_ascii_case(name))
        .then_some("Suspenso, ele congela a área de trabalho, e não haveria como retomar.")
}

/// Congela todas as threads do processo (`NtSuspendProcess`).
pub fn suspend(key: ProcKey, name: &str) -> Result<(), KillError> {
    if suspend_blocked(key.pid, name).is_some() {
        return Err(KillError::Protected);
    }
    nt_call(key, name, s!("NtSuspendProcess"))
}

pub fn resume(key: ProcKey, name: &str) -> Result<(), KillError> {
    nt_call(key, name, s!("NtResumeProcess"))
}

/// `NtSuspendProcess` e `NtResumeProcess` não estão no SDK: vêm do ntdll pelo nome.
fn nt_call(key: ProcKey, name: &str, function: windows::core::PCSTR) -> Result<(), KillError> {
    type NtProcessFn = unsafe extern "system" fn(HANDLE) -> i32;
    let guard = open_checked(key, name, PROCESS_SUSPEND_RESUME | PROCESS_QUERY_LIMITED_INFORMATION)?;
    unsafe {
        let ntdll = GetModuleHandleW(w!("ntdll.dll")).map_err(|e| KillError::Other(e.message()))?;
        let Some(address) = GetProcAddress(ntdll, function) else {
            return Err(KillError::Other("função do ntdll não encontrada".to_owned()));
        };
        let call: NtProcessFn = std::mem::transmute(address);
        match call(guard.0) {
            0 => Ok(()),
            // STATUS_ACCESS_DENIED
            status if status as u32 == 0xC000_0022 => Err(KillError::AccessDenied),
            status => Err(KillError::Other(format!("NTSTATUS 0x{:08X}", status as u32))),
        }
    }
}

/// Começo de `SYSTEM_PROCESS_INFORMATION` (x64), só os campos usados. A estrutura toda
/// tem 256 bytes e é seguida pelas threads do processo.
#[repr(C)]
struct ProcessEntry {
    next_entry_offset: u32,
    number_of_threads: u32,
    _skip: [u8; 72],
    unique_process_id: usize,
}

/// `SYSTEM_THREAD_INFORMATION` (x64), 80 bytes.
#[repr(C)]
struct ThreadEntry {
    _times_and_ids: [u8; 68],
    thread_state: u32,
    wait_reason: u32,
    _pad: u32,
}

const PROCESS_ENTRY_SIZE: usize = 256;
const THREAD_WAITING: u32 = 5;
const WAIT_SUSPENDED: u32 = 5;

/// PIDs com todas as threads suspensas: por este app, por outro programa ou pelo
/// Windows (app da loja em segundo plano). É o mesmo critério do Gerenciador de Tarefas.
/// Uma chamada só para o sistema todo; `buf` é reaproveitado entre coletas.
pub fn suspended_pids(buf: &mut Vec<u8>) -> std::collections::HashSet<u32> {
    type QueryFn = unsafe extern "system" fn(u32, *mut core::ffi::c_void, u32, *mut u32) -> i32;
    const SYSTEM_PROCESS_INFORMATION: u32 = 5;
    const STATUS_INFO_LENGTH_MISMATCH: u32 = 0xC000_0004;
    let mut out = std::collections::HashSet::new();
    unsafe {
        let Ok(ntdll) = GetModuleHandleW(w!("ntdll.dll")) else { return out };
        let Some(address) = GetProcAddress(ntdll, s!("NtQuerySystemInformation")) else { return out };
        let query: QueryFn = std::mem::transmute(address);
        if buf.len() < 512 * 1024 {
            buf.resize(512 * 1024, 0);
        }
        let mut status = 0;
        for _ in 0..4 {
            let mut needed = 0u32;
            status = query(SYSTEM_PROCESS_INFORMATION, buf.as_mut_ptr().cast(), buf.len() as u32, &mut needed);
            if status as u32 != STATUS_INFO_LENGTH_MISMATCH {
                break;
            }
            // Folga para processos que nascem entre uma chamada e outra.
            buf.resize((needed as usize).max(buf.len()) + 64 * 1024, 0);
        }
        if status != 0 {
            return out;
        }
        let mut offset = 0usize;
        loop {
            if offset + PROCESS_ENTRY_SIZE > buf.len() {
                break;
            }
            let entry = &*(buf.as_ptr().add(offset) as *const ProcessEntry);
            let threads = entry.number_of_threads as usize;
            let first = offset + PROCESS_ENTRY_SIZE;
            if threads > 0 && first + threads * size_of::<ThreadEntry>() <= buf.len() {
                let list = std::slice::from_raw_parts(buf.as_ptr().add(first) as *const ThreadEntry, threads);
                if list.iter().all(|t| t.thread_state == THREAD_WAITING && t.wait_reason == WAIT_SUSPENDED) {
                    out.insert(entry.unique_process_id as u32);
                }
            }
            if entry.next_entry_offset == 0 {
                break;
            }
            offset += entry.next_entry_offset as usize;
        }
    }
    out
}

/// Abre o processo e confere que o PID ainda é dele.
fn open(key: ProcKey, access: PROCESS_ACCESS_RIGHTS) -> Result<HandleGuard, KillError> {
    unsafe {
        let guard = HandleGuard(OpenProcess(access, false, key.pid).map_err(platform::open_error)?);
        if platform::pid_reused(platform::creation_time(guard.0), key.start) {
            return Err(KillError::PidReused);
        }
        Ok(guard)
    }
}

/// Como `open`, mas recusa processos críticos: mexer neles derruba o Windows.
fn open_checked(key: ProcKey, name: &str, access: PROCESS_ACCESS_RIGHTS) -> Result<HandleGuard, KillError> {
    if platform::is_protected_by_name(key.pid, name) {
        return Err(KillError::Protected);
    }
    let guard = open(key, access)?;
    let mut critical = BOOL(0);
    if unsafe { IsProcessCritical(guard.0, &mut critical) }.is_ok() && critical.as_bool() {
        return Err(KillError::Protected);
    }
    Ok(guard)
}

fn call_error(e: windows::core::Error) -> KillError {
    if e.code() == ERROR_ACCESS_DENIED.to_hresult() { KillError::AccessDenied } else { KillError::Other(e.message()) }
}

/// Núcleos ligados numa máscara, para exibir: "8 de 8".
pub fn core_count(mask: u64) -> u32 {
    mask.count_ones()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command};

    fn spawn_victim() -> Child {
        Command::new("ping").args(["-n", "30", "127.0.0.1"]).stdout(std::process::Stdio::null()).spawn().unwrap()
    }

    fn key_of(child: &Child) -> ProcKey {
        let pid = child.id();
        let mut sys = sysinfo::System::new();
        sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(pid)]), true);
        ProcKey { pid, start: sys.process(sysinfo::Pid::from_u32(pid)).unwrap().start_time() }
    }

    /// A suspensão chega às threads de forma assíncrona: com a máquina ocupada (a suíte
    /// inteira em paralelo), a primeira leitura logo depois ainda pode ver a thread rodando.
    fn settles(buf: &mut Vec<u8>, pid: u32, suspended: bool) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            if suspended_pids(buf).contains(&pid) == suspended {
                return true;
            }
            if std::time::Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn suspende_e_retoma() {
        let mut child = spawn_victim();
        let key = key_of(&child);
        let mut buf = Vec::new();
        assert!(!suspended_pids(&mut buf).contains(&key.pid));
        assert_eq!(suspend(key, "PING.EXE"), Ok(()));
        assert!(settles(&mut buf, key.pid, true), "suspenso por este app aparece na leitura do sistema");
        assert_eq!(resume(key, "PING.EXE"), Ok(()));
        assert!(settles(&mut buf, key.pid, false));
        // Hora de criação errada: o PID "é de outro processo".
        assert_eq!(suspend(ProcKey { pid: key.pid, start: key.start + 100 }, "PING.EXE"), Err(KillError::PidReused));
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn prioridade_afinidade_e_eficiencia() {
        let mut child = spawn_victim();
        let key = key_of(&child);
        // O processo herda a prioridade de quem o abriu: no runner do GitHub Actions o
        // job roda "abaixo do normal". Parte de Normal para não depender disso.
        set_priority(key, "PING.EXE", Priority::Normal).unwrap();
        assert_eq!(read(key).priority, Some(Priority::Normal));
        set_priority(key, "PING.EXE", Priority::BelowNormal).unwrap();
        assert_eq!(read(key).priority, Some(Priority::BelowNormal));

        let (_, system) = read(key).affinity.unwrap();
        let first = system.isolate_lowest_one();
        set_affinity(key, "PING.EXE", first).unwrap();
        assert_eq!(read(key).affinity, Some((first, system)));
        assert!(set_affinity(key, "PING.EXE", 0).is_err());

        set_efficiency(key, "PING.EXE", true).unwrap();
        let t = read(key);
        assert_eq!((t.efficiency, t.priority), (Some(true), Some(Priority::Idle)));
        set_efficiency(key, "PING.EXE", false).unwrap();
        assert_eq!(read(key).efficiency, Some(false));
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn nao_suspende_o_que_trava_a_sessao() {
        assert!(suspend_blocked(1234, "dwm.exe").is_some());
        assert!(suspend_blocked(1234, "csrss.exe").is_some());
        assert!(suspend_blocked(std::process::id(), "process-monitor.exe").is_some());
        assert!(suspend_blocked(1234, "node.exe").is_none());
    }
}
