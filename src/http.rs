//! Cliente HTTPS mínimo sobre o WinHTTP: o do próprio Windows, com o proxy do sistema,
//! sem dependência nova. Usado pela atualização automática e pela consulta de limites
//! de IA (`usage`).

use std::ffi::c_void;

use windows::Win32::Networking::WinHttp::{
    INTERNET_DEFAULT_HTTPS_PORT, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_OPTION_REDIRECT_POLICY,
    WINHTTP_OPTION_REDIRECT_POLICY_NEVER, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE, WinHttpCloseHandle,
    WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse,
    WinHttpSendRequest, WinHttpSetOption, WinHttpSetTimeouts,
};
use windows::core::{HSTRING, PCWSTR, w};

/// `WINHTTP_QUERY_RETRY_AFTER`: segundos ou data HTTP.
pub const QUERY_RETRY_AFTER: u32 = 36;

/// Handle do WinHTTP, fechado ao sair de escopo.
struct Handle(*mut c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
}

fn handle(raw: *mut c_void) -> Result<Handle, String> {
    if raw.is_null() { Err(describe(windows::core::Error::from_thread())) } else { Ok(Handle(raw)) }
}

pub struct Client {
    session: Handle,
}

pub struct Response<'a> {
    // Ordem de declaração = ordem de fechamento: a requisição antes da conexão.
    request: Handle,
    _connect: Handle,
    _client: std::marker::PhantomData<&'a Client>,
    pub status: u32,
}

impl Client {
    /// `timeouts_ms`: resolver o nome, conectar, enviar, receber.
    pub fn new(timeouts_ms: [i32; 4]) -> Result<Self, String> {
        let agent = HSTRING::from(format!("ProcessMonitor/{}", env!("CARGO_PKG_VERSION")));
        let session =
            handle(unsafe { WinHttpOpen(&agent, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0) })?;
        let [resolve, connect, send, receive] = timeouts_ms;
        unsafe { WinHttpSetTimeouts(session.0, resolve, connect, send, receive) }.map_err(describe)?;
        Ok(Self { session })
    }

    /// GET em `https://host/caminho`. Sem `follow`, um redirecionamento volta como
    /// resposta (para ler o `Location`).
    pub fn get(&self, url: &str, follow: bool) -> Result<Response<'_>, String> {
        self.get_with_headers(url, follow, &[])
    }

    /// GET com cabeçalhos extras (`("Authorization", "Bearer …")`). Os valores não vão
    /// para nenhuma mensagem de erro: podem ser credenciais.
    pub fn get_with_headers(&self, url: &str, follow: bool, headers: &[(&str, &str)]) -> Result<Response<'_>, String> {
        let rest = url.strip_prefix("https://").ok_or("só HTTPS")?;
        let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        let extra: Vec<u16> =
            headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect::<String>().encode_utf16().collect();
        unsafe {
            let connect = handle(WinHttpConnect(self.session.0, &HSTRING::from(host), INTERNET_DEFAULT_HTTPS_PORT, 0))?;
            let request = handle(WinHttpOpenRequest(
                connect.0,
                w!("GET"),
                &HSTRING::from(path),
                PCWSTR::null(),
                PCWSTR::null(),
                std::ptr::null(),
                WINHTTP_FLAG_SECURE,
            ))?;
            if !follow {
                let policy = WINHTTP_OPTION_REDIRECT_POLICY_NEVER.to_ne_bytes();
                WinHttpSetOption(Some(request.0), WINHTTP_OPTION_REDIRECT_POLICY, Some(&policy)).map_err(describe)?;
            }
            let extra = (!extra.is_empty()).then_some(extra.as_slice());
            WinHttpSendRequest(request.0, extra, None, 0, 0, 0).map_err(describe)?;
            WinHttpReceiveResponse(request.0, std::ptr::null_mut()).map_err(describe)?;
            let mut response = Response { request, _connect: connect, _client: std::marker::PhantomData, status: 0 };
            response.status = response.number(WINHTTP_QUERY_STATUS_CODE).ok_or("resposta sem código HTTP")?;
            Ok(response)
        }
    }
}

impl Response<'_> {
    pub fn number(&self, query: u32) -> Option<u32> {
        let mut value = 0u32;
        let mut len = size_of::<u32>() as u32;
        unsafe {
            WinHttpQueryHeaders(
                self.request.0,
                query | WINHTTP_QUERY_FLAG_NUMBER,
                PCWSTR::null(),
                Some((&raw mut value).cast()),
                &mut len,
                std::ptr::null_mut(),
            )
        }
        .ok()?;
        Some(value)
    }

    pub fn header(&self, query: u32) -> Option<String> {
        let mut buf = vec![0u16; 2048];
        let mut len = (buf.len() * 2) as u32;
        unsafe {
            WinHttpQueryHeaders(self.request.0, query, PCWSTR::null(), Some(buf.as_mut_ptr().cast()), &mut len, std::ptr::null_mut())
        }
        .ok()?;
        Some(String::from_utf16_lossy(&buf[..len as usize / 2]))
    }

    pub fn read_each(&self, mut f: impl FnMut(&[u8]) -> Result<(), String>) -> Result<(), String> {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let mut read = 0u32;
            unsafe { WinHttpReadData(self.request.0, buf.as_mut_ptr().cast(), buf.len() as u32, &mut read) }.map_err(describe)?;
            if read == 0 {
                return Ok(());
            }
            f(&buf[..read as usize])?;
        }
    }

    /// O corpo inteiro, até `limit` bytes (respostas de API são pequenas; um corpo maior
    /// que isso não é a resposta esperada).
    pub fn body(&self, limit: usize) -> Result<Vec<u8>, String> {
        let mut body = Vec::new();
        self.read_each(|chunk| {
            body.extend_from_slice(chunk);
            if body.len() > limit { Err("resposta grande demais".to_owned()) } else { Ok(()) }
        })?;
        Ok(body)
    }
}

/// Os erros do WinHTTP não têm texto no FormatMessage do sistema: os comuns em português.
pub fn describe(e: windows::core::Error) -> String {
    match e.code().0 as u32 & 0xFFFF {
        12002 => "o servidor não respondeu a tempo".to_owned(),
        12007 | 12029 => "sem conexão com o servidor".to_owned(),
        12030 | 12031 => "a conexão caiu".to_owned(),
        12175 | 12038 | 12044 | 12045 => "falha no certificado HTTPS".to_owned(),
        code => format!("erro de rede {code}"),
    }
}
