# Process Monitor

Monitor de sistema para Windows em Rust puro: CPU, memória, disco, rede, GPU e
processos, com finalizar processo. Só Windows (usa Win32 direto). Interface e
comentários em pt-BR, com acentos.

Stack: Rust 2024 · eframe/egui 0.36 (renderizador `glow`) · sysinfo 0.39 ·
windows 0.62 · instalador Inno Setup 6.

Repositório: `FernandoGarciaSimpipe/process-monitor`, na conta **pessoal** do
Fernando, privado. **Nunca** na org `gpdsimpipe`.

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

## Windows em português

O Windows devolve nomes localizados: a conta `S-1-5-18` é "SISTEMA", as descrições
de arquivo vêm traduzidas. Testes não podem supor nomes em inglês.

# Visual

- Tokens em `src/theme.rs`: superfícies grafite que sobem poucos pontos por nível,
  bordas em linhas finas translúcidas, uma cor por recurso (CPU, memória, disco,
  rede, GPU). Carga é sempre calor âmbar (`theme::heat_fill`), em qualquer coluna.
- Assinatura: o ponto luminoso na ponta de todo gráfico (valor mais recente), que
  se repete no logo.
- Fontes do próprio Windows (Segoe UI, Segoe UI Semibold, Cascadia Mono) e ícones
  Phosphor. Nada de fonte embutida no binário.
- A máquina de desenvolvimento tem tela de 1366×768: a janela abre dentro da área
  útil, e os cinco cartões da barra lateral (76 px) cabem sem rolar.
