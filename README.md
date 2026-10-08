# Process Monitor

Monitor de sistema para Windows, em Rust puro ([egui](https://github.com/emilk/egui)).
Acompanha CPU, memória, disco, rede e GPU, mostra os detalhes de cada processo e
permite finalizá-los.

![ícone](assets/app.png)

## O que faz

- **Desempenho:** barra lateral com um mini-gráfico por recurso; cada um abre uma tela
  com gráfico de 60 amostras e estatísticas (CPU por núcleo, unidades de disco,
  adaptadores de rede, VRAM).
- **Processos:** nome amigável (a descrição do executável), ícone, PID, usuário e uso
  de CPU, memória, disco e GPU, com calor âmbar nas células conforme a carga. Busca
  por nome, PID, caminho ou linha de comando (Ctrl+F; vírgula separa termos),
  ordenação por coluna e agrupamento por executável.
- **Detalhes:** caminho, linha de comando, diretório de trabalho, processo pai e
  filhos, início e tempo de execução.
- **Finalizar:** processo, árvore ou grupo, sempre com confirmação. Antes de finalizar,
  confere se o PID ainda é do mesmo processo; processos críticos do Windows ficam
  bloqueados. Sem administrador, processos do sistema não podem ser finalizados — o
  botão **Executar como administrador** reabre o app elevado.

## Desenvolvimento

Requer Rust (MSVC) e as ferramentas de C++ do Visual Studio.

```powershell
.\dev.ps1            # compila e abre em debug, com console para logs
.\dev.ps1 -Release   # build otimizado
cargo test           # inclui testes que criam e finalizam processos reais
```

## Instalador

Requer [Inno Setup 6](https://jrsoftware.org/isinfo.php)
(`winget install --id JRSoftware.InnoSetup -e`).

```powershell
.\package.ps1        # gera dist\ProcessMonitor-Setup-<versão>.exe
```

O instalador é por usuário (`%LOCALAPPDATA%\Programs\Process Monitor`), não pede
administrador e cria atalhos na área de trabalho e no Menu Iniciar.

## Estrutura

| Arquivo | Papel |
|---|---|
| `src/sampler.rs` | Thread de coleta (sysinfo), histórico e árvore de processos |
| `src/gpu.rs` | Uso de GPU pelos contadores de desempenho do Windows (PDH) e DXGI |
| `src/platform.rs` | Win32: finalizar com verificação, elevação, ícones, descrição do arquivo |
| `src/theme.rs` | Tokens visuais e configuração do egui |
| `src/ui/` | Barra lateral, tabela de processos, detalhes e telas de desempenho |
| `installer/` | Script do Inno Setup |
