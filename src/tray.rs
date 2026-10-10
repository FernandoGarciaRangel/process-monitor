//! Ícone na área de notificação, instância única e encerramento pelo instalador.
//!
//! Uma thread mantém uma janela de nível superior oculta (não `HWND_MESSAGE`): só
//! janelas de nível superior recebem `WM_QUERYENDSESSION`/`WM_ENDSESSION`, que é como
//! o Restart Manager do instalador pede para o app fechar. Com o X escondendo a
//! janela, sem isso a atualização ficaria esperando.
//!
//! A janela principal é do eframe: a thread só levanta sinais e pede um quadro. O
//! `logic` continua rodando com a janela escondida e aplica os sinais.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, LRESULT, POINT, WAIT_ABANDONED, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
use windows::Win32::UI::Shell::{
    ExtractIconExW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_LARGE_ICON, NIIF_USER, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NIM_SETVERSION, NIN_BALLOONHIDE, NIN_BALLOONTIMEOUT, NIN_BALLOONUSERCLICK, NIN_SELECT,
    NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, AppendMenuW, ASFW_ANY, ChangeWindowMessageFilterEx, CreatePopupMenu, CreateWindowExW,
    DefWindowProcW, DestroyMenu, DispatchMessageW, FindWindowW, GetCursorPos, GetMessageW, HICON, MF_SEPARATOR, MF_STRING,
    MSG, MSGFLT_ALLOW, PostMessageW, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow, TPM_BOTTOMALIGN,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, WINDOW_EX_STYLE, WM_APP, WM_CONTEXTMENU, WM_ENDSESSION, WM_NULL,
    WM_QUERYENDSESSION, WNDCLASSW, WS_OVERLAPPED,
};
use windows::core::{HSTRING, PCWSTR, w};

// O build de debug tem nomes próprios: rodar o dev com a versão instalada aberta não
// pode cair na instância única da outra.
#[cfg(debug_assertions)]
const SUFFIX: &str = ".dev";
#[cfg(not(debug_assertions))]
const SUFFIX: &str = "";

/// Callback do ícone (`uCallbackMessage`).
const WM_TRAY: u32 = WM_APP + 1;
/// `wParam` 1 mostra o ícone, 0 tira.
const WM_SET_ICON: u32 = WM_APP + 2;
/// `lParam` é um `Box<Balloon>` cedido pela thread que chamou [`Notifier::notify`].
const WM_NOTIFY_BALLOON: u32 = WM_APP + 3;

const MENU_OPEN: usize = 1;
const MENU_QUIT: usize = 2;

fn class_name() -> HSTRING {
    HSTRING::from(format!("ProcessMonitor.Tray{SUFFIX}"))
}

fn show_message() -> u32 {
    unsafe { RegisterWindowMessageW(&HSTRING::from(format!("ProcessMonitor.Show{SUFFIX}"))) }
}

/// Posse do mutex da instância única; fica viva até o fim do `main`.
pub struct Instance(#[allow(dead_code)] HANDLE);

/// `None` se já houver outra instância: ela é avisada para aparecer e esta deve sair.
/// Com `replace` (reinício como administrador), espera a anterior fechar.
pub fn single_instance(replace: bool) -> Option<Instance> {
    let name = HSTRING::from(format!("Local\\ProcessMonitor.8F3C2A51{SUFFIX}"));
    let mutex = unsafe { CreateMutexW(None, true, &name) };
    let exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    match mutex {
        Ok(handle) if !exists => Some(Instance(handle)),
        Ok(handle) if replace => {
            let wait = unsafe { WaitForSingleObject(handle, 15_000) };
            (wait == WAIT_OBJECT_0 || wait == WAIT_ABANDONED).then_some(Instance(handle))
        }
        // Já existe, ou acesso negado: o mutex é de uma instância elevada.
        _ => {
            unsafe {
                if let Ok(hwnd) = FindWindowW(&class_name(), PCWSTR::null()) {
                    // A instância que já roda precisa poder trazer a janela para a frente.
                    let _ = AllowSetForegroundWindow(ASFW_ANY);
                    let _ = PostMessageW(Some(hwnd), show_message(), WPARAM(0), LPARAM(0));
                }
            }
            None
        }
    }
}

struct Shared {
    ctx: egui::Context,
    /// O usuário quer o ícone (opção "Minimizar para a bandeja").
    icon_wanted: AtomicBool,
    show: AtomicBool,
    quit: AtomicBool,
    balloon_clicked: AtomicBool,
}

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

/// Lado do app: lê os sinais da thread da bandeja.
pub struct Tray {
    shared: Arc<Shared>,
    notifier: Notifier,
}

/// Mandar balões e mudar o ícone de qualquer thread.
#[derive(Clone, Copy)]
pub struct Notifier {
    hwnd: isize,
}

struct Balloon {
    title: String,
    body: String,
}

impl Notifier {
    /// Balão do Windows (vira notificação no Windows 10 e 11). Se o ícone estiver
    /// desligado, ele aparece só enquanto o balão estiver na tela.
    pub fn notify(&self, title: &str, body: &str) {
        let balloon = Box::into_raw(Box::new(Balloon { title: title.to_owned(), body: body.to_owned() }));
        let posted =
            unsafe { PostMessageW(Some(self.hwnd()), WM_NOTIFY_BALLOON, WPARAM(0), LPARAM(balloon as isize)) };
        if posted.is_err() {
            // Ninguém vai receber: devolve a memória aqui.
            drop(unsafe { Box::from_raw(balloon) });
        }
    }

    fn hwnd(&self) -> HWND {
        HWND(self.hwnd as *mut _)
    }
}

impl Tray {
    pub fn spawn(ctx: egui::Context, icon: bool) -> Self {
        let shared = SHARED
            .get_or_init(|| {
                Arc::new(Shared {
                    ctx,
                    icon_wanted: AtomicBool::new(icon),
                    show: AtomicBool::new(false),
                    quit: AtomicBool::new(false),
                    balloon_clicked: AtomicBool::new(false),
                })
            })
            .clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("tray".into())
            .spawn(move || run(tx))
            .expect("thread da bandeja");
        let hwnd = rx.recv().unwrap_or(0);
        Self { shared, notifier: Notifier { hwnd } }
    }

    pub fn notifier(&self) -> Notifier {
        self.notifier
    }

    pub fn set_icon(&self, on: bool) {
        self.shared.icon_wanted.store(on, Ordering::Relaxed);
        unsafe {
            let _ = PostMessageW(Some(self.notifier.hwnd()), WM_SET_ICON, WPARAM(on as usize), LPARAM(0));
        }
    }

    /// Pedido de mostrar a janela (clique no ícone, segunda instância, balão).
    pub fn take_show(&self) -> bool {
        self.shared.show.swap(false, Ordering::Relaxed)
    }

    /// "Sair" no menu, ou o Windows/instalador pedindo para fechar.
    pub fn quit_requested(&self) -> bool {
        self.shared.quit.load(Ordering::Relaxed)
    }

    /// O usuário clicou no último balão. Devolve `true` uma vez por clique.
    #[allow(dead_code, reason = "para os alertas, que abrem a tela do aviso ao clicar")]
    pub fn take_balloon_click(&self) -> bool {
        self.shared.balloon_clicked.swap(false, Ordering::Relaxed)
    }
}

fn shared() -> &'static Shared {
    SHARED.get().expect("bandeja iniciada")
}

fn signal(flag: &AtomicBool) {
    flag.store(true, Ordering::Relaxed);
    shared().ctx.request_repaint();
}

/// Estado que só a thread da bandeja toca.
struct Local {
    hwnd: HWND,
    small: HICON,
    large: HICON,
    shown: bool,
    /// Ícone posto só para um balão, com a opção desligada.
    temporary: bool,
    taskbar_created: u32,
    show_message: u32,
}

thread_local! {
    static LOCAL: std::cell::RefCell<Option<Local>> = const { std::cell::RefCell::new(None) };
}

fn run(tx: std::sync::mpsc::Sender<isize>) {
    unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        let class = class_name();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            &class,
            w!("Process Monitor Tray"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        ) else {
            let _ = tx.send(0);
            return;
        };

        // Uma instância elevada ainda precisa ouvir a segunda instância comum.
        let show_message = show_message();
        let _ = ChangeWindowMessageFilterEx(hwnd, show_message, MSGFLT_ALLOW, None);
        let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
        let _ = ChangeWindowMessageFilterEx(hwnd, taskbar_created, MSGFLT_ALLOW, None);

        let exe = std::env::current_exe().map(|p| HSTRING::from(p.as_os_str())).unwrap_or_default();
        let (mut large, mut small) = (HICON::default(), HICON::default());
        ExtractIconExW(&exe, 0, Some(&mut large), Some(&mut small), 1);

        LOCAL.with_borrow_mut(|l| {
            *l = Some(Local { hwnd, small, large, shown: false, temporary: false, taskbar_created, show_message })
        });
        if shared().icon_wanted.load(Ordering::Relaxed) {
            with_local(add_icon);
        }
        let _ = tx.send(hwnd.0 as isize);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }
    }
}

fn with_local<R>(f: impl FnOnce(&mut Local) -> R) -> Option<R> {
    LOCAL.with_borrow_mut(|l| l.as_mut().map(f))
}

fn base_data(l: &Local) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW { cbSize: size_of::<NOTIFYICONDATAW>() as u32, hWnd: l.hwnd, uID: 1, ..Default::default() }
}

fn copy_wide(dst: &mut [u16], text: &str) {
    let wide: Vec<u16> = text.encode_utf16().take(dst.len() - 1).collect();
    dst[..wide.len()].copy_from_slice(&wide);
    dst[wide.len()] = 0;
}

fn add_icon(l: &mut Local) {
    let mut data = base_data(l);
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    data.uCallbackMessage = WM_TRAY;
    data.hIcon = l.small;
    copy_wide(&mut data.szTip, "Process Monitor");
    unsafe {
        if Shell_NotifyIconW(NIM_ADD, &data).as_bool() {
            data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &data);
            l.shown = true;
        }
    }
}

fn remove_icon(l: &mut Local) {
    if l.shown {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &base_data(l));
        }
        l.shown = false;
    }
    l.temporary = false;
}

fn show_balloon(l: &mut Local, balloon: &Balloon) {
    if !l.shown {
        add_icon(l);
        l.temporary = l.shown;
    }
    let mut data = base_data(l);
    data.uFlags = NIF_INFO;
    data.dwInfoFlags = NIIF_USER | NIIF_LARGE_ICON;
    data.hBalloonIcon = l.large;
    copy_wide(&mut data.szInfoTitle, &balloon.title);
    copy_wide(&mut data.szInfo, &balloon.body);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
    }
}

fn menu(hwnd: HWND) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let _ = AppendMenuW(menu, MF_STRING, MENU_OPEN, w!("Abrir o Process Monitor"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, MENU_QUIT, w!("Sair"));
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // Sem isso o menu não fecha ao clicar fora (documentado no TrackPopupMenu).
        let _ = SetForegroundWindow(hwnd);
        let choice =
            TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, None, hwnd, None).0 as usize;
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        match choice {
            MENU_OPEN => signal(&shared().show),
            MENU_QUIT => signal(&shared().quit),
            _ => {}
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let (taskbar_created, show_message) = with_local(|l| (l.taskbar_created, l.show_message)).unwrap_or((0, 0));
    match msg {
        WM_TRAY => {
            // NOTIFYICON_VERSION_4: o evento vem na palavra baixa do lParam.
            match (lparam.0 as u32) & 0xFFFF {
                NIN_SELECT | 0x0401 /* NIN_KEYSELECT */ => signal(&shared().show),
                WM_CONTEXTMENU => menu(hwnd),
                NIN_BALLOONUSERCLICK => {
                    shared().balloon_clicked.store(true, Ordering::Relaxed);
                    signal(&shared().show);
                    with_local(|l| if l.temporary { remove_icon(l) });
                }
                NIN_BALLOONTIMEOUT | NIN_BALLOONHIDE => {
                    with_local(|l| if l.temporary { remove_icon(l) });
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_SET_ICON => {
            with_local(|l| match (wparam.0 != 0, l.shown && !l.temporary) {
                (true, false) => {
                    l.temporary = false;
                    if !l.shown {
                        add_icon(l);
                    }
                }
                (false, true) => remove_icon(l),
                _ => {}
            });
            LRESULT(0)
        }
        WM_NOTIFY_BALLOON => {
            let balloon = unsafe { Box::from_raw(lparam.0 as *mut Balloon) };
            with_local(|l| show_balloon(l, &balloon));
            LRESULT(0)
        }
        // O Restart Manager (instalador) e o logoff: concorda e sai de verdade.
        WM_QUERYENDSESSION => LRESULT(1),
        WM_ENDSESSION => {
            if wparam.0 != 0 {
                signal(&shared().quit);
                with_local(remove_icon);
            }
            LRESULT(0)
        }
        _ if msg != 0 && msg == show_message => {
            signal(&shared().show);
            LRESULT(0)
        }
        // O Explorer reiniciou: os ícones da bandeja somem e precisam ser postos de novo.
        _ if msg != 0 && msg == taskbar_created => {
            with_local(|l| {
                let wanted = shared().icon_wanted.load(Ordering::Relaxed);
                l.shown = false;
                l.temporary = false;
                if wanted {
                    add_icon(l);
                }
            });
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Tira o ícone ao sair, para não ficar um fantasma até o mouse passar por cima.
/// Direto daqui, não pela fila da thread: o processo termina logo em seguida.
impl Drop for Tray {
    fn drop(&mut self) {
        let data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.notifier.hwnd(),
            uID: 1,
            ..Default::default()
        };
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &data);
        }
    }
}
