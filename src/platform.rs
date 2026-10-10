//! Chamadas Win32 que o sysinfo não cobre: finalizar com verificação de PID,
//! processos críticos, elevação, Explorer, navegador e ícones de executáveis.

use std::os::windows::process::CommandExt;
use std::path::Path;

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_INVALID_PARAMETER, FILETIME, HANDLE, HLOCAL, LocalFree, RECT,
};
use windows::Win32::Security::Authorization::ConvertStringSidToSidW;
use windows::Win32::Security::{LookupAccountSidW, PSID, SID_NAME_USE};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, GetDIBits, GetObjectW, HBITMAP, HDC, HGDIOBJ,
};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAGS_AND_ATTRIBUTES, GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, GetProcessTimes, IsProcessCritical, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, TerminateProcess,
};
use windows::Win32::UI::Shell::{SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGetFileInfoW, ShellExecuteW};
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, GetIconInfo, HICON, ICONINFO, SPI_GETWORKAREA, SW_SHOWNORMAL, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    SystemParametersInfoW,
};
use windows::core::{BOOL, HSTRING, PCWSTR, PWSTR, w};

/// Processos que derrubam o Windows (tela azul ou logoff) se forem finalizados.
const CRITICAL_NAMES: &[&str] = &[
    "csrss.exe",
    "wininit.exe",
    "winlogon.exe",
    "smss.exe",
    "services.exe",
    "lsass.exe",
    "system",
    "registry",
    "memory compression",
    "secure system",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KillError {
    AccessDenied,
    NotFound,
    /// O PID existe, mas pertence a outro processo que nasceu depois do que foi listado.
    PidReused,
    Protected,
    Other(String),
}

impl KillError {
    pub fn describe(&self) -> &str {
        match self {
            KillError::AccessDenied => "acesso negado",
            KillError::NotFound => "o processo já tinha encerrado",
            KillError::PidReused => "o PID agora pertence a outro processo",
            KillError::Protected => "processo crítico do sistema",
            KillError::Other(msg) => msg,
        }
    }
}

/// Finaliza o processo `pid`, mas só se a hora de criação ainda for `expected_start`
/// (segundos Unix, como o sysinfo devolve). O handle aberto segura o processo, então
/// entre a conferência e o `TerminateProcess` o PID não troca de dono.
pub fn kill(pid: u32, expected_start: u64, name: &str) -> Result<(), KillError> {
    if is_protected_by_name(pid, name) {
        return Err(KillError::Protected);
    }
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
            .map_err(open_error)?;
        let guard = HandleGuard(handle);

        // Processo que já saiu, mas cujo objeto ainda existe porque alguém mantém um
        // handle aberto: o TerminateProcess daria "acesso negado", o que confunde.
        const STILL_ACTIVE: u32 = 259;
        let mut exit_code = 0u32;
        if GetExitCodeProcess(guard.0, &mut exit_code).is_ok() && exit_code != STILL_ACTIVE {
            return Err(KillError::NotFound);
        }

        let mut critical = BOOL(0);
        if IsProcessCritical(guard.0, &mut critical).is_ok() && critical.as_bool() {
            return Err(KillError::Protected);
        }

        if let Some(created) = creation_time(guard.0)
            && created.abs_diff(expected_start) > 2
        {
            return Err(KillError::PidReused);
        }

        TerminateProcess(guard.0, 1).map_err(|e| {
            if e.code() == ERROR_ACCESS_DENIED.to_hresult() {
                KillError::AccessDenied
            } else {
                KillError::Other(e.message())
            }
        })
    }
}

/// Proteção que não precisa de handle: PID 0 e 4, o próprio app e os nomes críticos.
pub fn is_protected_by_name(pid: u32, name: &str) -> bool {
    pid == 0
        || pid == 4
        || pid == std::process::id()
        || CRITICAL_NAMES.iter().any(|n| n.eq_ignore_ascii_case(name))
}

/// Proteção completa: nomes conhecidos ou o sinalizador de processo crítico do kernel.
pub fn is_protected(pid: u32, name: &str) -> bool {
    if is_protected_by_name(pid, name) {
        return true;
    }
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let guard = HandleGuard(handle);
        let mut critical = BOOL(0);
        IsProcessCritical(guard.0, &mut critical).is_ok() && critical.as_bool()
    }
}

pub fn is_elevated() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let guard = HandleGuard(token);
        let mut elevation = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        GetTokenInformation(
            guard.0,
            TokenElevation,
            Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
        .is_ok()
            && elevation.TokenIsElevated != 0
    }
}

/// Abre uma nova instância elevada (UAC). `Ok` quando o usuário aceitou; o chamador
/// fecha a instância atual. `--replace` faz a nova esperar esta sair, em vez de cair
/// na instância única.
pub fn relaunch_as_admin() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe = HSTRING::from(exe.as_os_str());
    let result = unsafe { ShellExecuteW(None, w!("runas"), &exe, w!("--replace"), PCWSTR::null(), SW_SHOWNORMAL) };
    // ShellExecute devolve um valor > 32 em caso de sucesso.
    if result.0 as isize > 32 {
        Ok(())
    } else {
        Err("elevação cancelada".to_owned())
    }
}

const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const RUN_VALUE: PCWSTR = w!("Process Monitor");
/// Argumento que a entrada de inicialização passa para abrir sem mostrar a janela.
pub const MINIMIZED_ARG: &str = "--minimized";

/// Comando registrado para abrir com o Windows (`HKCU\...\Run`), se houver.
pub fn autostart_command() -> Option<String> {
    unsafe {
        let mut len = 0u32;
        if RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, RRF_RT_REG_SZ, None, None, Some(&mut len)).is_err() {
            return None;
        }
        let mut buf = vec![0u16; len as usize / 2];
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            RUN_VALUE,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut len),
        )
        .ok()
        .ok()?;
        Some(String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)]))
    }
}

/// Liga ou desliga a abertura com o Windows, apontando para este executável. Por
/// usuário: não pede administrador.
pub fn set_autostart(enabled: bool, minimized: bool) -> Result<(), String> {
    unsafe {
        if !enabled {
            let result = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE);
            return if result.is_ok() || result == ERROR_FILE_NOT_FOUND { Ok(()) } else { Err(result.to_hresult().message()) };
        }
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut command = format!("\"{}\"", exe.display());
        if minimized {
            command.push(' ');
            command.push_str(MINIMIZED_ARG);
        }
        let wide: Vec<u16> = command.encode_utf16().chain([0]).collect();
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            RUN_VALUE,
            REG_SZ.0,
            Some(wide.as_ptr().cast()),
            (wide.len() * 2) as u32,
        )
        .ok()
        .map_err(|e| e.message())
    }
}

/// Nome amigável do executável: o campo "Descrição" das propriedades do arquivo
/// (`claude.exe` → "Claude Code"). `None` se o arquivo não tiver informação de versão.
pub fn file_description(path: &Path) -> Option<String> {
    unsafe {
        let file = HSTRING::from(path.as_os_str());
        let size = GetFileVersionInfoSizeW(&file, None);
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        GetFileVersionInfoW(&file, None, size, data.as_mut_ptr().cast()).ok()?;
        let block = data.as_ptr().cast::<core::ffi::c_void>();

        // Primeiro os idiomas que o próprio arquivo declara; depois inglês e português
        // como reserva, porque há executáveis com a tabela de traduções errada.
        let mut tables = Vec::new();
        let mut ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut len = 0u32;
        if VerQueryValueW(block, w!(r"\VarFileInfo\Translation"), &mut ptr, &mut len).as_bool() && len >= 4 {
            let pairs = std::slice::from_raw_parts(ptr.cast::<u16>(), (len / 2) as usize);
            for pair in pairs.as_chunks::<2>().0 {
                tables.push(format!("{:04x}{:04x}", pair[0], pair[1]));
            }
        }
        tables.extend(["040904b0", "040904e4", "041604b0", "041604e4"].map(String::from));

        for table in tables {
            let key = HSTRING::from(format!(r"\StringFileInfo\{table}\FileDescription"));
            if VerQueryValueW(block, &key, &mut ptr, &mut len).as_bool() && len > 1 {
                // `len` conta caracteres, incluindo o nulo final.
                let chars = std::slice::from_raw_parts(ptr.cast::<u16>(), len as usize);
                let text = String::from_utf16_lossy(chars);
                let text = text.trim_matches(char::from(0)).trim();
                if !text.is_empty() {
                    return Some(text.to_owned());
                }
            }
        }
        None
    }
}

/// Área útil do monitor principal (sem a barra de tarefas), em pontos lógicos.
pub fn work_area() -> Option<egui::Rect> {
    unsafe {
        let mut rect = RECT::default();
        SystemParametersInfoW(SPI_GETWORKAREA, 0, Some((&mut rect as *mut RECT).cast()), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0))
            .ok()?;
        // Antes do winit ativar o DPI awareness o Windows já devolve valores lógicos
        // (e 96 dpi); depois, físicos e o dpi real. A divisão serve aos dois casos.
        let scale = GetDpiForSystem().max(96) as f32 / 96.0;
        let to_pt = |v: i32| v as f32 / scale;
        Some(egui::Rect::from_min_max(
            egui::pos2(to_pt(rect.left), to_pt(rect.top)),
            egui::pos2(to_pt(rect.right), to_pt(rect.bottom)),
        ))
    }
}

/// Abre o Explorer com o arquivo selecionado.
pub fn open_in_explorer(path: &Path) {
    let _ = std::process::Command::new("explorer.exe")
        .raw_arg(format!("/select,\"{}\"", path.display()))
        .spawn();
}

/// Abre uma URL no navegador padrão, ou uma pasta no Explorer.
pub fn shell_open(target: &str) {
    unsafe {
        ShellExecuteW(None, w!("open"), &HSTRING::from(target), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

/// Roda `cmd` numa janela de console nova, em `cwd`, com `title` na barra. A janela
/// fica aberta (`cmd /k`) se o servidor cair, para o erro continuar à vista.
pub fn spawn_in_console(cmd: &str, cwd: &Path, title: &str) -> Result<(), String> {
    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
    std::process::Command::new("cmd.exe")
        .raw_arg(console_args(cmd, title))
        .current_dir(cwd)
        .creation_flags(CREATE_NEW_CONSOLE)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Argumentos do `cmd.exe`: `/k "title <título> & <comando>"`. Com `/k` seguido de aspas,
/// o cmd tira só o primeiro e o último caractere, então as aspas do comando sobrevivem.
fn console_args(cmd: &str, title: &str) -> String {
    // No título, os caracteres especiais do cmd encerrariam o comando no meio.
    let title: String = title.chars().filter(|c| !"&|<>^\"%()".contains(*c)).collect();
    format!("/k \"title {} & {cmd}\"", title.trim())
}

/// Ícone grande (32 px) do executável em RGBA, ou `None` se o arquivo não tiver ícone.
/// Precisa de COM inicializado na thread; chame [`init_com_for_icons`] antes.
pub fn extract_icon(path: &Path) -> Option<([usize; 2], Vec<u8>)> {
    unsafe {
        let wide = HSTRING::from(path.as_os_str());
        let mut info = SHFILEINFOW::default();
        let ok = SHGetFileInfoW(
            &wide,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut info),
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        );
        if ok == 0 || info.hIcon.is_invalid() {
            return None;
        }
        let pixels = icon_to_rgba(info.hIcon);
        let _ = DestroyIcon(info.hIcon);
        pixels
    }
}

pub fn init_com_for_icons() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
}

unsafe fn icon_to_rgba(icon: HICON) -> Option<([usize; 2], Vec<u8>)> {
    unsafe {
        let mut info = ICONINFO::default();
        GetIconInfo(icon, &mut info).ok()?;
        let color = info.hbmColor;
        let mask = info.hbmMask;
        let dc = CreateCompatibleDC(None);

        let result = (|| {
            if color.is_invalid() {
                return None;
            }
            let mut bmp = BITMAP::default();
            if GetObjectW(HGDIOBJ(color.0), size_of::<BITMAP>() as i32, Some((&mut bmp as *mut BITMAP).cast())) == 0 {
                return None;
            }
            let (w, h) = (bmp.bmWidth.max(0) as usize, bmp.bmHeight.max(0) as usize);
            if w == 0 || h == 0 {
                return None;
            }
            let mut pixels = read_bitmap_bgra(dc, color, w, h)?;
            // Ícones antigos não têm canal alfa: a transparência vem da máscara.
            if pixels.as_chunks::<4>().0.iter().all(|p| p[3] == 0) {
                let mask_px = read_bitmap_bgra(dc, mask, w, h)?;
                for (p, m) in pixels.as_chunks_mut::<4>().0.iter_mut().zip(mask_px.as_chunks::<4>().0) {
                    p[3] = if m[0] == 0 { 255 } else { 0 };
                }
            }
            for p in pixels.as_chunks_mut::<4>().0 {
                p.swap(0, 2); // BGRA → RGBA
            }
            Some(([w, h], pixels))
        })();

        let _ = DeleteDC(dc);
        if !color.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(color.0));
        }
        if !mask.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(mask.0));
        }
        result
    }
}

unsafe fn read_bitmap_bgra(dc: HDC, bitmap: HBITMAP, w: usize, h: usize) -> Option<Vec<u8>> {
    unsafe {
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32), // negativo: linhas de cima para baixo
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut buf = vec![0u8; w * h * 4];
        let lines = GetDIBits(dc, bitmap, 0, h as u32, Some(buf.as_mut_ptr().cast()), &mut bmi, DIB_RGB_COLORS);
        (lines == h as i32).then_some(buf)
    }
}

/// Nome da conta de um SID em texto (`S-1-5-18` → `SYSTEM`). `None` se o SID não resolver.
pub fn account_name(sid: &str) -> Option<String> {
    unsafe {
        let mut psid = PSID::default();
        ConvertStringSidToSidW(&HSTRING::from(sid), &mut psid).ok()?;
        let mut name = [0u16; 256];
        let mut domain = [0u16; 256];
        let (mut name_len, mut domain_len) = (name.len() as u32, domain.len() as u32);
        let mut kind = SID_NAME_USE::default();
        let found = LookupAccountSidW(
            PCWSTR::null(),
            psid,
            Some(PWSTR(name.as_mut_ptr())),
            &mut name_len,
            Some(PWSTR(domain.as_mut_ptr())),
            &mut domain_len,
            &mut kind,
        );
        LocalFree(Some(HLOCAL(psid.0)));
        found.ok()?;
        Some(String::from_utf16_lossy(&name[..name_len as usize]))
    }
}

/// Hora de criação em segundos Unix.
unsafe fn creation_time(handle: HANDLE) -> Option<u64> {
    let (mut created, mut exit, mut kernel, mut user) =
        (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    unsafe { GetProcessTimes(handle, &mut created, &mut exit, &mut kernel, &mut user).ok()? };
    filetime_to_unix(created)
}

/// FILETIME (intervalos de 100 ns desde 1601-01-01) em segundos Unix.
pub fn filetime_to_unix(time: FILETIME) -> Option<u64> {
    let ticks = ((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64;
    const UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;
    ticks.checked_sub(UNIX_EPOCH_TICKS).map(|t| t / 10_000_000)
}

fn open_error(e: windows::core::Error) -> KillError {
    if e.code() == ERROR_ACCESS_DENIED.to_hresult() {
        KillError::AccessDenied
    } else if e.code() == ERROR_INVALID_PARAMETER.to_hresult() {
        KillError::NotFound
    } else {
        KillError::Other(e.message())
    }
}

struct HandleGuard(HANDLE);

impl Drop for HandleGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command};
    use std::time::Duration;

    /// Mexe no registro do usuário: `cargo test autostart -- --ignored`. Não roda se
    /// já houver uma entrada, para não apagar a do app instalado.
    #[test]
    #[ignore]
    fn autostart_roundtrip() {
        if autostart_command().is_some() {
            eprintln!("já existe uma entrada de inicialização; teste pulado");
            return;
        }
        set_autostart(true, true).unwrap();
        let cmd = autostart_command().expect("entrada gravada");
        assert!(cmd.starts_with('"') && cmd.ends_with(MINIMIZED_ARG), "{cmd}");
        set_autostart(true, false).unwrap();
        assert!(!autostart_command().unwrap().contains(MINIMIZED_ARG));
        set_autostart(false, false).unwrap();
        assert_eq!(autostart_command(), None);
        // Desligar de novo, sem entrada, não é erro.
        set_autostart(false, false).unwrap();
    }

    /// Processo descartável que fica vivo ~30 s se ninguém o finalizar.
    fn spawn_victim() -> Child {
        Command::new("ping").args(["-n", "30", "127.0.0.1"]).spawn().expect("ping")
    }

    fn start_time_of(pid: u32) -> u64 {
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).expect("abrir processo");
            let guard = HandleGuard(h);
            creation_time(guard.0).expect("hora de criação")
        }
    }

    #[test]
    fn finaliza_processo_do_usuario() {
        let mut child = spawn_victim();
        let pid = child.id();
        let start = start_time_of(pid);
        assert_eq!(kill(pid, start, "PING.EXE"), Ok(()));
        let status = child.wait().expect("esperar");
        assert!(!status.success(), "o ping deveria ter sido interrompido");
    }

    #[test]
    fn recusa_pid_reaproveitado() {
        let mut child = spawn_victim();
        let pid = child.id();
        let start = start_time_of(pid);
        // Hora de criação diferente da real: outro processo com o mesmo PID.
        assert_eq!(kill(pid, start.saturating_sub(3600), "PING.EXE"), Err(KillError::PidReused));
        assert!(child.try_wait().expect("try_wait").is_none(), "não pode ter sido finalizado");
        let _ = child.kill();
    }

    #[test]
    fn processo_que_ja_saiu() {
        let mut child = spawn_victim();
        let pid = child.id();
        let start = start_time_of(pid);
        child.kill().unwrap();
        child.wait().unwrap();
        // O `child` ainda segura um handle: o processo saiu, mas o objeto existe.
        assert_eq!(kill(pid, start, "PING.EXE"), Err(KillError::NotFound));
        drop(child);
        std::thread::sleep(Duration::from_millis(100));
        assert!(matches!(kill(pid, start, "PING.EXE"), Err(KillError::NotFound | KillError::PidReused)));
    }

    #[test]
    fn protege_processos_criticos() {
        assert_eq!(kill(4, 0, "System"), Err(KillError::Protected));
        assert_eq!(kill(std::process::id(), 0, "process-monitor.exe"), Err(KillError::Protected));
        assert!(is_protected(999_999, "csrss.exe"));
        assert!(is_protected(999_999, "LSASS.EXE"));
        assert!(!is_protected(999_999, "notepad.exe"));
    }

    #[test]
    fn icone_de_executavel() {
        init_com_for_icons();
        let notepad = Path::new(r"C:\Windows\System32\notepad.exe");
        let (size, rgba) = extract_icon(notepad).expect("notepad tem ícone");
        assert!(size[0] >= 16 && size[1] >= 16);
        assert_eq!(rgba.len(), size[0] * size[1] * 4);
        assert!(rgba.as_chunks::<4>().0.iter().any(|p| p[3] > 0), "ícone não pode ser todo transparente");
    }

    #[test]
    fn descricao_do_executavel() {
        let explorer = Path::new(r"C:\Windows\explorer.exe");
        let description = file_description(explorer).expect("explorer.exe tem descrição");
        assert!(!description.is_empty() && !description.contains('\0'), "{description:?}");
        assert!(file_description(Path::new(r"C:\nao\existe.exe")).is_none());
    }

    #[test]
    fn conta_do_sistema() {
        // O nome é localizado: "SYSTEM" em inglês, "SISTEMA" em português.
        let name = account_name("S-1-5-18").expect("S-1-5-18 sempre resolve");
        assert!(name.to_uppercase().starts_with("SIS") || name.to_uppercase().starts_with("SYS"), "{name}");
        assert!(account_name("isto não é um SID").is_none());
    }

    #[test]
    fn linha_do_console() {
        assert_eq!(
            console_args(r#""C:\nodejs\node.exe" npm-cli.js run dev"#, "web & api (5173)"),
            r#"/k "title web  api 5173 & "C:\nodejs\node.exe" npm-cli.js run dev""#
        );
    }

    #[test]
    #[ignore = "abre uma janela de console; rode com cargo test console -- --ignored"]
    fn console_novo_roda_no_diretorio() {
        // `cmd /k` abriria uma janela que fica aberta; aqui o comando sai sozinho com `exit`.
        let dir = std::env::temp_dir().join(format!("process-monitor-console-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("rodou.txt");
        spawn_in_console("echo ok> rodou.txt & exit", &dir, "teste").expect("spawn");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(marker.exists(), "o comando deveria ter rodado em {dir:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn area_util_da_tela() {
        let area = work_area().expect("área útil");
        assert!(area.width() > 300.0 && area.height() > 200.0);
    }
}
