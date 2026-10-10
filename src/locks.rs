//! Quem está usando um arquivo ou pasta, pelo Restart Manager do Windows: é o mesmo
//! mecanismo que o instalador usa para saber o que fechar. Pega arquivos abertos e
//! também executáveis e DLLs carregados (o `.exe` de um app aberto em `target\release`).
//!
//! Não precisa de administrador, mas só enxerga processos da mesma sessão e dos que o
//! usuário pode consultar.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::RestartManager::{
    CCH_RM_SESSION_KEY, RM_PROCESS_INFO, RmEndSession, RmGetList, RmRegisterResources, RmStartSession,
};
use windows::core::{HSTRING, PCWSTR, PWSTR};

use crate::platform;

/// Numa pasta, quantos arquivos verificar no máximo; `target\` passa de centenas de milhares.
const MAX_FILES: usize = 10_000;
/// Arquivos por sessão do Restart Manager: com muitos de uma vez, a consulta fica lenta.
const BATCH: usize = 1_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Locker {
    pub pid: u32,
    /// Hora de criação em segundos Unix, como o sysinfo: com o PID, identifica o processo.
    pub start: u64,
    /// Nome que o Restart Manager dá (título da janela ou descrição); serve quando o
    /// processo não está na lista da coleta.
    pub app_name: String,
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub lockers: Vec<Locker>,
    /// Arquivos verificados.
    pub files: usize,
    /// A pasta tinha mais que [`MAX_FILES`] arquivos e o resto ficou de fora.
    pub truncated: bool,
}

/// Processos que mantêm abertos os arquivos de `paths` (ou de dentro das pastas).
pub fn find(paths: &[PathBuf]) -> Result<Report, String> {
    let mut files = Vec::new();
    let mut truncated = false;
    for path in paths {
        collect(path, &mut files, &mut truncated);
    }
    if files.is_empty() {
        return Err(if paths.iter().any(|p| p.exists()) {
            "A pasta não tem arquivos.".to_owned()
        } else {
            "Arquivo ou pasta não encontrado.".to_owned()
        });
    }
    let mut found: BTreeMap<(u32, u64), Locker> = BTreeMap::new();
    for chunk in files.chunks(BATCH) {
        for locker in session(chunk)? {
            found.entry((locker.pid, locker.start)).or_insert(locker);
        }
    }
    Ok(Report { lockers: found.into_values().collect(), files: files.len(), truncated })
}

/// Arquivos de `path`: ele mesmo, ou os de dentro da pasta, sem seguir links.
fn collect(path: &Path, out: &mut Vec<PathBuf>, truncated: &mut bool) {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return };
    if !meta.is_dir() {
        push(path.to_path_buf(), out, truncated);
        return;
    }
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                stack.push(entry.path());
            } else if !push(entry.path(), out, truncated) {
                return;
            }
        }
    }
}

/// Guarda o arquivo; `false` quando o limite foi atingido.
fn push(file: PathBuf, out: &mut Vec<PathBuf>, truncated: &mut bool) -> bool {
    if out.len() >= MAX_FILES {
        *truncated = true;
        return false;
    }
    out.push(file);
    true
}

/// Fecha a sessão do Restart Manager em qualquer saída.
struct Session(u32);

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            let _ = RmEndSession(self.0);
        }
    }
}

fn check(result: WIN32_ERROR, what: &str) -> Result<(), String> {
    if result == ERROR_SUCCESS { Ok(()) } else { Err(format!("{what} falhou (código {})", result.0)) }
}

fn session(files: &[PathBuf]) -> Result<Vec<Locker>, String> {
    unsafe {
        let mut handle = 0u32;
        let mut key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
        check(RmStartSession(&mut handle, None, PWSTR(key.as_mut_ptr())), "Abrir a sessão do Restart Manager")?;
        let session = Session(handle);

        let names: Vec<HSTRING> = files.iter().map(|f| HSTRING::from(f.as_path())).collect();
        let ptrs: Vec<PCWSTR> = names.iter().map(|n| PCWSTR(n.as_ptr())).collect();
        check(RmRegisterResources(session.0, Some(&ptrs), None, None), "Registrar os arquivos")?;

        // A lista pode crescer entre a consulta do tamanho e a leitura: tenta de novo.
        let mut infos: Vec<RM_PROCESS_INFO> = Vec::new();
        for _ in 0..4 {
            let (mut needed, mut count, mut reasons) = (0u32, infos.len() as u32, 0u32);
            let ptr = (!infos.is_empty()).then_some(infos.as_mut_ptr());
            let result = RmGetList(session.0, &mut needed, &mut count, ptr, &mut reasons);
            if result == ERROR_MORE_DATA {
                infos = vec![RM_PROCESS_INFO::default(); needed as usize + 4];
                continue;
            }
            check(result, "Listar os processos")?;
            infos.truncate(count as usize);
            return Ok(infos.iter().map(locker_of).collect());
        }
        Err("Listar os processos falhou: a lista não parava de mudar".to_owned())
    }
}

fn locker_of(info: &RM_PROCESS_INFO) -> Locker {
    let name = &info.strAppName;
    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    Locker {
        pid: info.Process.dwProcessId,
        start: platform::filetime_to_unix(info.Process.ProcessStartTime).unwrap_or(0),
        app_name: String::from_utf16_lossy(&name[..len]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::os::windows::fs::OpenOptionsExt;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("process-monitor-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn acha_o_proprio_processo_segurando_o_arquivo() {
        let dir = temp_dir("locks");
        let file = dir.join("sub").join("preso.txt");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, b"x").unwrap();
        // Sem compartilhamento: é o caso que impede outro programa de abrir ou apagar.
        let held = OpenOptions::new().read(true).share_mode(0).open(&file).unwrap();
        let me = std::process::id();

        let report = find(std::slice::from_ref(&file)).expect("consulta");
        assert!(report.lockers.iter().any(|l| l.pid == me), "{report:?}");
        // Pela pasta também acha, procurando nos arquivos de dentro.
        let report = find(std::slice::from_ref(&dir)).expect("consulta da pasta");
        assert_eq!(report.files, 1);
        assert!(report.lockers.iter().any(|l| l.pid == me && l.start > 0), "{report:?}");

        drop(held);
        let report = find(std::slice::from_ref(&file)).expect("consulta");
        assert!(!report.lockers.iter().any(|l| l.pid == me), "solto, não pode aparecer: {report:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn acha_quem_esta_rodando_o_executavel() {
        // O caso do `target\release\app.exe` que não deixa o build sobrescrever.
        let mut child = std::process::Command::new("ping").args(["-n", "30", "127.0.0.1"]).spawn().expect("ping");
        let exe = PathBuf::from(r"C:\Windows\System32\PING.EXE");
        let report = find(std::slice::from_ref(&exe)).expect("consulta");
        let _ = child.kill();
        let _ = child.wait();
        assert!(report.lockers.iter().any(|l| l.pid == child.id()), "{report:?}");
    }

    #[test]
    fn caminho_inexistente() {
        assert!(find(&[PathBuf::from(r"C:\nao\existe\nada.txt")]).is_err());
    }

    #[test]
    fn pasta_grande_para_no_limite() {
        let dir = temp_dir("limite");
        let mut files = Vec::new();
        let mut truncated = false;
        for i in 0..3 {
            fs::write(dir.join(format!("{i}.txt")), b"").unwrap();
        }
        // Começa quase cheio: só cabe mais um.
        files.resize(MAX_FILES - 1, PathBuf::new());
        collect(&dir, &mut files, &mut truncated);
        assert_eq!(files.len(), MAX_FILES);
        assert!(truncated);
        let _ = fs::remove_dir_all(&dir);
    }
}
