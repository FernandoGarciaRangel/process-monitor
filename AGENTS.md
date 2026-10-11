# Process Monitor

Monitor de sistema para Windows em Rust puro: CPU, memória, disco, rede, GPU e
processos, com finalizar processo. Só Windows (usa Win32 direto). Interface e
comentários em pt-BR, com acentos.

Stack: Rust 2024 · eframe/egui 0.36 (renderizador `glow`) · sysinfo 0.39 ·
windows 0.62 · instalador Inno Setup 6.

Repositório: `FernandoGarciaRangel/process-monitor`, na conta **pessoal** do
Fernando. **Nunca** na org `gpdsimpipe`. Precisa ser **público**: a atualização
automática baixa as releases sem token (ver `updater::REPO`).

# Build e testes

## Comandos

| Comando | O que faz |
|---|---|
| `.\dev.ps1` | Compila e abre em debug, no terminal atual (com console para panics e logs) |
| `.\dev.ps1 -Release` | Build otimizado, sem console |
| `cargo test` | Inclui testes que **criam e finalizam processos reais** (`ping`) |
| `cargo clippy --all-targets -- -D warnings` | Tem que sair limpo |
| `.\package.ps1` | Build de release + Inno Setup → `dist\ProcessMonitor-Setup-<versão>.exe` |
| `scripts\make-icon.ps1` | Regera `assets\app.png` e `assets\app.ico` (só se o desenho mudar) |

Serviços e o app sobem **no terminal atual**: nada de `Start-Process wt` ou outra
janela de terminal, porque fora do Orca o log se perde.

O rustup instala em `~\.cargo\bin`, que não está no PATH de terminais abertos antes
da instalação. Os scripts acrescentam o caminho sozinhos; em comandos avulsos, use
`$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"`.

## 🔴 App aberto trava o build e o instalador

- Com o app rodando de `target\release`, o `cargo build --release` não sobrescreve o exe.
- Para atualizar a versão instalada sem interface:
  `dist\ProcessMonitor-Setup-<v>.exe /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /CLOSEAPPLICATIONS`.
  Se uma instância estiver **elevada** (o usuário clicou em "Executar como
  administrador"), o instalador não consegue fechá-la e sai com código 5. Não
  finalize a instância do usuário: peça para ele fechar.

## Versão e release

A versão vem só do `Cargo.toml`; o `package.ps1` repassa ao Inno Setup. Para
publicar: subir a versão, `.\package.ps1`, depois
`gh release create vX.Y.Z dist\ProcessMonitor-Setup-X.Y.Z.exe --notes-file <arquivo>`.
O instalador não é assinado; as notas avisam do SmartScreen.

## Nunca commitar

`target/` e `dist/`. O instalador vai como anexo da release, não para o git.

# Armadilhas

## 🔴 PowerShell 5.1 e UTF-8

O shell padrão aqui é o Windows PowerShell 5.1, que trata arquivo sem BOM como ANSI.

- Não reescreva código com `Get-Content -Raw` + `[IO.File]::WriteAllText`: os acentos
  saem com codificação dupla (`ç` → `Ã§`). Use a ferramenta de edição ou
  `New-Object System.Text.UTF8Encoding $false` explícito na leitura e na escrita.
- Todo `.ps1` com texto acentuado precisa de **UTF-8 com BOM**.
- Não use `$ErrorActionPreference = 'Stop'` em script que chama cargo: o progresso
  vai para stderr e vira erro terminante quando a saída é redirecionada. Cheque
  `$LASTEXITCODE`.
- Mensagem de commit com acento: escreva num arquivo UTF-8 e use `git commit -F`.

## egui 0.36 não é o egui da memória

A API mudou bastante nas últimas versões. O trait `App` tem `logic(ctx)` e
`ui(ui)` (não `update`). Os painéis são `egui::Panel::left/right/bottom(id).show(ui, …)`.
`Modal` e `Area` recebem `&Context`. Antes de usar uma API que você não viu neste
código, leia o fonte em `~\.cargo\registry\src\*\egui-0.36.2`.

## 🔴 Painel lateral que cresce sozinho

Um painel redimensionável cresce até o `size_range` máximo se o conteúdo for mais
largo que ele, e cresce de novo a cada quadro. Duas causas já aconteceram:

- Largura dividida em partes iguais (`(disponível - gap) / 2`): o arredondamento
  soma meio pixel por item. Use `.floor() - 1.0`.
- Texto longo sem espaços (caminho, linha de comando): com quebra só por palavra,
  ele empurra a largura. Use `widgets::long_text`, que quebra em qualquer caractere.

## Renderizador: `glow`, não `wgpu`

O padrão do eframe 0.36 é `wgpu`, e com ele o app ocupava 379 MB; com `glow`, 118 MB.
Um monitor de sistema não pode ser o processo pesado da lista. Não reative as
features padrão do eframe.

## GPU vem do PDH, não do sysinfo

O sysinfo 0.39 publicado não tem GPU; a feature `gpu` que aparece na documentação
é do branch principal. `src/gpu.rs` lê `GPU Engine` e `GPU Adapter Memory` com
`PdhAddEnglishCounterW`, porque o Windows é pt-BR e os nomes dos contadores são
localizados.

## 🔴 Identidade de processo

- PID sozinho é reaproveitado. A chave é `ProcKey` (PID + hora de criação), e o
  `platform::kill` confere a hora de criação (±2 s) e o código de saída (processo
  zumbi vira "já tinha encerrado", não "acesso negado") antes do `TerminateProcess`.
- `ProcInfo.name` é o **nome do executável**: é a chave dos grupos e o que a
  proteção de processos críticos confere (`CRITICAL_NAMES` + `IsProcessCritical`).
  `display_name()` (a descrição do arquivo, "Claude Code") é só para exibir. Nunca
  passe o nome amigável para `platform::kill` ou `is_protected`.

## Coleta fora da thread da UI

A thread do `sampler` é dona do sysinfo e publica um `Arc<Snapshot>`; a UI só lê e
repinta quando chega dado novo. Não faça chamada de sistema por quadro na UI
(`is_protected` tem cache). Meta: o app abaixo de 2% de CPU com intervalo de 1 s.

## Portas: sondagem HTTP

`src/ports.rs` lê as portas com `GetExtendedTcpTable` na thread de coleta; o GET
de cada uma roda na thread `ports`, só com a tela de portas (ou o painel de
detalhes de um processo com porta) aberta.

- Conecte no endereço em que a porta escuta (`Listener::connect_addr`): o Vite no
  Windows escuta só em `::1`, e `127.0.0.1` é recusado.
- O timeout de leitura é 4 s de propósito: servidor parado há dias tem a memória
  paginada e demora na primeira resposta. Com 1,5 s ele aparecia como calado.
- Python só é sondado com indício de servidor web na linha de comando: kernel do
  Jupyter e `debugpy` escutam portas que não falam HTTP.
- 🔴 Não mande GET a cada rodada. uvicorn, Flask, Django e o Next registram toda
  requisição, e o terminal do servidor do usuário enchia de `GET / 404` a cada 2 s.
  A rodada de 2 s só abre e fecha a conexão TCP (`check`); o GET completo roda na
  primeira vez, quando a porta volta depois de cair e a cada `FULL_INTERVAL` (30 s).

## Atualização automática

`src/updater.rs`, numa thread própria, com WinHTTP (sem dependência nova). Lê a
versão do redirecionamento de `github.com/<repo>/releases/latest` e baixa
`ProcessMonitor-Setup-X.Y.Z.exe` da release `vX.Y.Z` para
`%LOCALAPPDATA%\Process Monitor\updates`. Só instala quando o usuário clica.

- 🔴 O nome do anexo e a tag são contrato: mudou o `OutputBaseFilename` do `.iss`
  ou o padrão da tag, as versões já instaladas param de achar a atualização.
- O app roda o instalador com `/VERYSILENT … /RELAUNCH=1` e fecha; o `[Code]` do
  `.iss` lê `/RELAUNCH` e abre o app no fim. Sem isso, a instalação silenciosa não
  reabre nada (`skipifsilent`).
- Release sem token: o repositório tem que ser público para isso funcionar.
- Teste de rede (ignorado por padrão): `cargo test updater -- --ignored`.

## 🔴 Bandeja e instância única

`src/tray.rs`: uma thread com uma janela de nível superior **oculta** (classe
`ProcessMonitor.Tray`, título "Process Monitor Tray"), que cuida do ícone, do menu
e dos balões, e recebe o pedido de fechar do Windows.

- Com "Minimizar para a bandeja" (padrão), o X só esconde. Para encerrar de verdade,
  use `App::quit` (menu Sair, instalador, atualização, reinício como administrador).
  Um `ViewportCommand::Close` solto cai na bandeja.
- O instalador fecha o app pelo Restart Manager (`WM_QUERYENDSESSION` com
  `ENDSESSION_CLOSEAPP`), que só chega a janelas de nível superior. Não troque a
  janela da bandeja por `HWND_MESSAGE`: o X que esconde travaria a atualização.
- Instância única por mutex: uma segunda cópia avisa a primeira e sai na hora. Num
  script, espere o processo anterior sumir antes de abrir outro. O build de debug
  usa nomes com `.dev`, então convive com a versão instalada (mas não com outro
  debug). `--replace` espera a anterior fechar; `--minimized` abre só na bandeja.
- Com a janela escondida, o eframe continua chamando `logic`: é por ali que ela
  volta (`ViewportCommand::Visible`).
- Para testar o fechamento pelo instalador sem instalar: X (`WM_CLOSE`) para esconder
  e depois `RmShutdown` sem forçar no caminho do exe. Mandar `WM_QUERYENDSESSION`
  na mão não reproduz o Restart Manager e dá falso negativo.

## Alertas

`src/alerts.rs` é só a regra (testável, sem chamada de sistema); quem avalia é a
thread do `sampler`, a cada coleta, porque ela continua viva com a janela escondida.
Os avisos saem por `tray::Notifier`; o alvo do último fica em `App::last_alert`
para o clique no balão abrir o processo ou a porta.

- Porta vigiada só "cai" depois de 3 s fora (reinício rápido não avisa) e só se já
  foi vista de pé. CPU e memória avisam uma vez por processo, com histerese.
- O próprio Process Monitor nunca se denuncia.

## Quem está usando um arquivo

`src/locks.rs` usa o Restart Manager (o mesmo do instalador): pega arquivos abertos
e executáveis em execução, sem administrador. O resultado vem com PID e hora de
criação; case com a coleta pelos dois (±2 s), nunca só pelo PID.

## Windows em português

O Windows devolve nomes localizados: a conta `S-1-5-18` é "SISTEMA", as descrições
de arquivo vêm traduzidas. Testes não podem supor nomes em inglês.

# Visual

- Cores do Fernando: **preto e laranja #FA5019**. Tokens em `src/theme.rs`:
  superfícies pretas sem matiz que sobem poucos pontos por nível, bordas em linhas
  finas translúcidas. O laranja (`theme::ACCENT`) marca a interface (seleção, item
  ativo, links, logo e ícone do app) e a carga (`theme::heat_fill`), em qualquer
  coluna. Cada recurso (CPU, memória, disco, rede, GPU) mantém a sua cor nos
  gráficos. `DANGER` é carmim, para não se confundir com o laranja.
- Assinatura: o ponto luminoso na ponta de todo gráfico (valor mais recente), que
  se repete no logo.
- Fontes do próprio Windows (Segoe UI, Segoe UI Semibold, Cascadia Mono) e ícones
  Phosphor. Nada de fonte embutida no binário.
- A máquina de desenvolvimento tem tela de 1366×768: a janela abre dentro da área
  útil, e os cinco cartões da barra lateral (76 px) cabem sem rolar.
