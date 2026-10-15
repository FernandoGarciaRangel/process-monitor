//! Programas que abrem com o Windows: as chaves `Run` do registro e as pastas
//! Inicializar. Ligar e desligar usa o mesmo lugar que o Gerenciador de Tarefas
//! (`Explorer\StartupApproved`), então os dois concordam sobre o estado de cada um.
//!
//! A leitura é rápida (algumas chaves e duas pastas): roda na thread da UI, só ao abrir
//! a tela e depois de uma mudança, nunca por quadro.

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_SUCCESS, FILETIME};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY, REG_BINARY, REG_EXPAND_SZ, REG_SZ,
    REG_VALUE_TYPE, RRF_RT_REG_BINARY, RegCloseKey, RegEnumValueW, RegGetValueW, RegOpenKeyExW, RegSetKeyValueW,
};
use windows::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
use windows::core::{HSTRING, PWSTR};

const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN32: &str = r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Run";
const APPROVED: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// `HKCU\...\Run`: só deste usuário.
    UserRun,
    /// `HKLM\...\Run`: todos os usuários; mudar pede administrador.
    MachineRun,
    /// `HKLM\...\WOW6432Node\...\Run`: programas de 32 bits, todos os usuários.
    MachineRun32,
    UserFolder,
    CommonFolder,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::UserRun => "Registro · usuário",
            Source::MachineRun | Source::MachineRun32 => "Registro · todos os usuários",
            Source::UserFolder => "Pasta Inicializar · usuário",
            Source::CommonFolder => "Pasta Inicializar · todos os usuários",
        }
    }

    /// Mudar pede administrador.
    pub fn machine(self) -> bool {
        matches!(self, Source::MachineRun | Source::MachineRun32 | Source::CommonFolder)
    }

    fn approved(self) -> (HKEY, String) {
        let (root, sub) = match self {
            Source::UserRun => (HKEY_CURRENT_USER, "Run"),
            Source::MachineRun => (HKEY_LOCAL_MACHINE, "Run"),
            Source::MachineRun32 => (HKEY_LOCAL_MACHINE, "Run32"),
            Source::UserFolder => (HKEY_CURRENT_USER, "StartupFolder"),
            Source::CommonFolder => (HKEY_LOCAL_MACHINE, "StartupFolder"),
        };
        (root, format!(r"{APPROVED}\{sub}"))
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    /// Nome do valor no registro, ou do arquivo na pasta: é a chave do `StartupApproved`.
    pub name: String,
    /// Linha de comando (registro) ou caminho do atalho (pasta).
    pub command: String,
    pub source: Source,
    pub enabled: bool,
    /// Executável tirado do comando, para o ícone, a descrição e saber se está rodando.
    pub exe: Option<PathBuf>,
    pub description: Option<String>,
}

impl Entry {
    pub fn display_name(&self) -> &str {
        self.description.as_deref().unwrap_or(&self.name)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    AccessDenied,
    Other(String),
}

impl Error {
    pub fn describe(&self) -> &str {
        match self {
            Error::AccessDenied => "acesso negado",
            Error::Other(msg) => msg,
        }
    }
}

/// Todas as entradas, na ordem: usuário, máquina, pastas.
pub fn list() -> Vec<Entry> {
    let mut out = Vec::new();
    for (root, key, source) in [
        (HKEY_CURRENT_USER, RUN, Source::UserRun),
        (HKEY_LOCAL_MACHINE, RUN, Source::MachineRun),
        (HKEY_LOCAL_MACHINE, RUN32, Source::MachineRun32),
    ] {
        for (name, command) in run_values(root, key) {
            let exe = exe_of(&command);
            out.push(entry(name, command, source, exe));
        }
    }
    for (dir, source) in [(user_folder(), Source::UserFolder), (common_folder(), Source::CommonFolder)] {
        let Some(dir) = dir else { continue };
        let Ok(files) = std::fs::read_dir(&dir) else { continue };
        for file in files.flatten() {
            let path = file.path();
            let name = file.file_name().to_string_lossy().into_owned();
            if !path.is_file() || name.eq_ignore_ascii_case("desktop.ini") {
                continue;
            }
            let command = path.display().to_string();
            out.push(entry(name, command, source, Some(path)));
        }
    }
    out
}

fn entry(name: String, command: String, source: Source, exe: Option<PathBuf>) -> Entry {
    let enabled = approved(source, &name);
    let description = exe
        .as_deref()
        .filter(|e| e.extension().is_some_and(|x| x.eq_ignore_ascii_case("exe")))
        .and_then(crate::platform::file_description);
    Entry { name, command, source, enabled, exe, description }
}

/// Liga ou desliga, como o Gerenciador de Tarefas: 02 ligado, 03 desligado, e a hora.
pub fn set_enabled(entry: &Entry, on: bool) -> Result<(), Error> {
    set_enabled_raw(entry.source, &entry.name, on)
}

/// O mesmo, só com o que a cópia elevada recebe pela linha de comando.
pub fn set_enabled_raw(source: Source, name: &str, on: bool) -> Result<(), Error> {
    let (root, key) = source.approved();
    let mut data = [0u8; 12];
    data[0] = if on { 2 } else { 3 };
    if !on {
        let time: FILETIME = unsafe { GetSystemTimeAsFileTime() };
        data[4..8].copy_from_slice(&time.dwLowDateTime.to_le_bytes());
        data[8..12].copy_from_slice(&time.dwHighDateTime.to_le_bytes());
    }
    let result = unsafe {
        RegSetKeyValueW(
            root,
            &HSTRING::from(key),
            &HSTRING::from(name),
            REG_BINARY.0,
            Some(data.as_ptr().cast()),
            data.len() as u32,
        )
    };
    match result {
        r if r == ERROR_SUCCESS => Ok(()),
        r if r == ERROR_ACCESS_DENIED => Err(Error::AccessDenied),
        r => Err(Error::Other(r.to_hresult().message())),
    }
}

/// Sem valor no `StartupApproved`, a entrada está ligada. Byte 0 par = ligada.
fn approved(source: Source, name: &str) -> bool {
    let (root, key) = source.approved();
    let mut data = [0u8; 12];
    let mut len = data.len() as u32;
    let result = unsafe {
        RegGetValueW(
            root,
            &HSTRING::from(key),
            &HSTRING::from(name),
            RRF_RT_REG_BINARY,
            None,
            Some(data.as_mut_ptr().cast()),
            Some(&mut len),
        )
    };
    result != ERROR_SUCCESS || len == 0 || data[0] % 2 == 0
}

/// Valores de texto de uma chave `Run`, com as variáveis de ambiente expandidas.
fn run_values(root: HKEY, key: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    unsafe {
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(root, &HSTRING::from(key), None, KEY_READ | KEY_WOW64_64KEY, &mut hkey) != ERROR_SUCCESS {
            return out;
        }
        let mut name = vec![0u16; 16_384];
        let mut data = vec![0u8; 8192];
        for index in 0.. {
            let mut name_len = name.len() as u32;
            let mut data_len = data.len() as u32;
            let mut kind = 0u32;
            let result = RegEnumValueW(
                hkey,
                index,
                Some(PWSTR(name.as_mut_ptr())),
                &mut name_len,
                None,
                Some(&mut kind),
                Some(data.as_mut_ptr()),
                Some(&mut data_len),
            );
            if result != ERROR_SUCCESS {
                // ERROR_MORE_DATA: valor gigante, que não é um comando de verdade; pula.
                if result.0 == 234 {
                    continue;
                }
                break;
            }
            let kind = REG_VALUE_TYPE(kind);
            if kind != REG_SZ && kind != REG_EXPAND_SZ {
                continue;
            }
            let value_name = String::from_utf16_lossy(&name[..name_len as usize]);
            let chars: Vec<u16> =
                data[..data_len as usize].as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
            let command = String::from_utf16_lossy(&chars).trim_end_matches('\0').trim().to_owned();
            if !command.is_empty() {
                out.push((value_name, expand_env(&command)));
            }
        }
        let _ = RegCloseKey(hkey);
    }
    out
}

/// `%LOCALAPPDATA%\x` → caminho real. Variável desconhecida fica como está.
fn expand_env(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) => {
                let var = &after[..end];
                match std::env::var(var) {
                    Ok(value) if !var.is_empty() => out.push_str(&value),
                    _ => {
                        out.push('%');
                        out.push_str(var);
                        out.push('%');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Executável de uma linha de comando: o trecho entre aspas, ou até o `.exe`.
fn exe_of(command: &str) -> Option<PathBuf> {
    let command = command.trim();
    if let Some(quoted) = command.strip_prefix('"') {
        return quoted.split_once('"').map(|(exe, _)| PathBuf::from(exe));
    }
    let lower = command.to_ascii_lowercase();
    match lower.find(".exe") {
        Some(i) => Some(PathBuf::from(&command[..i + 4])),
        None => command.split_whitespace().next().map(PathBuf::from),
    }
}

fn user_folder() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|d| Path::new(&d).join(r"Microsoft\Windows\Start Menu\Programs\Startup"))
}

fn common_folder() -> Option<PathBuf> {
    std::env::var_os("ProgramData").map(|d| Path::new(&d).join(r"Microsoft\Windows\Start Menu\Programs\StartUp"))
}

/// Pasta onde a entrada está, para "Abrir local".
pub fn location(entry: &Entry) -> Option<PathBuf> {
    match entry.source {
        Source::UserFolder | Source::CommonFolder => Some(PathBuf::from(&entry.command)),
        _ => entry.exe.clone().filter(|e| e.exists()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executavel_da_linha_de_comando() {
        assert_eq!(exe_of(r#""C:\Program Files\App\app.exe" --min"#), Some(PathBuf::from(r"C:\Program Files\App\app.exe")));
        assert_eq!(exe_of(r"C:\Windows\system32\SecurityHealthSystray.exe"), Some(PathBuf::from(r"C:\Windows\system32\SecurityHealthSystray.exe")));
        assert_eq!(exe_of(r"C:\Tools\x.EXE /background"), Some(PathBuf::from(r"C:\Tools\x.EXE")));
        assert_eq!(exe_of("rundll32 shell32.dll,Foo"), Some(PathBuf::from("rundll32")));
    }

    #[test]
    fn variaveis_de_ambiente() {
        let windir = std::env::var("windir").unwrap();
        assert_eq!(expand_env(r"%windir%\x.exe"), format!(r"{windir}\x.exe"));
        assert_eq!(expand_env("%NAO_EXISTE_XYZ%\\a"), "%NAO_EXISTE_XYZ%\\a");
        assert_eq!(expand_env("100% certo"), "100% certo");
    }

    #[test]
    fn le_as_entradas() {
        // Não dá para supor quais existem; só que a leitura não falha e não repete.
        let entries = list();
        let mut keys: Vec<_> = entries.iter().map(|e| (e.source as u8, e.name.to_lowercase())).collect();
        keys.sort();
        let before = keys.len();
        keys.dedup();
        assert_eq!(before, keys.len());
    }
}
