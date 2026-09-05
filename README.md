# xdir

Gerenciador gráfico de arquivos para Linux/X11, escrito em Rust.

## Visão

O **xdir** nasce da pergunta: como seria um gerenciador de arquivos gráfico se
tivesse sido projetado a partir das ideias de navegação, contexto e eficiência
de file managers de terminal (Yazi, Ranger) — em vez de copiar Explorer,
Nautilus, Dolphin ou Thunar?

A proposta não é colocar uma TUI dentro de uma janela. É uma GUI de verdade
(thumbnails, previews ricos, drag-and-drop, clipboard, menus contextuais,
seleção com mouse) onde o teclado é tão poderoso quanto o mouse.

O xdir funciona de forma completamente independente. Em um ambiente com
[xomposite](#) e [xbar](#) ele pode oferecer integrações opcionais via IPC e
hints explícitos, mas nenhum dos dois é requisito.

## Princípios

1. **GUI de verdade, keyboard-first (não keyboard-only).** Mouse e teclado
   acessam o mesmo conjunto de ações internas.
2. **Layout assinatura de três painéis:** `parent → current → preview`
   (contexto → conteúdo atual → conteúdo selecionado).
3. **Tiling interno.** Painéis divisíveis vertical/horizontalmente, cada um
   navegando um diretório independente — sem reimplementar um window manager.
4. **Preview como API central**, não um complemento — providers plugáveis
   (texto, código, imagem, PDF, e futuramente vídeo/áudio/archive/planilha).
5. **Filesystem desacoplado da UI.** Operações destrutivas nunca são chamadas
   diretamente por handlers de UI; passam por uma camada de comandos
   (`Command → FileOperation → Filesystem`) que futuramente suporta fila,
   progresso, cancelamento e undo.
6. **Openers configuráveis explicitamente** (TOML), sem depender apenas do
   banco MIME do desktop.
7. **Integrações com xbar/xomposite são sempre opcionais**, publicadas via
   IPC (Unix socket) e hints explícitos. Nenhuma das duas é dependência.

Detalhes completos de arquitetura, operações, openers e integrações estão em
[`docs/VISION.md`](docs/VISION.md).

## Roadmap

O desenvolvimento segue milestones incrementais, do núcleo sem UI até tiling,
drag-and-drop, thumbnails e IPC. Veja [`docs/ROADMAP.md`](docs/ROADMAP.md).

Regra de ouro para qualquer feature nova:

> Isso torna a navegação e manipulação de arquivos mais rápida, ou estamos
> apenas transformando o xdir em outro desktop environment?

Se a resposta for a segunda opção, não implementar.

## Tecnologia

- **Linguagem:** Rust.
- **Toolkit gráfico:** a definir após spike técnico (candidatos: GTK4, iced).
- Sem Electron, Chromium ou WebView como UI principal.
- Sem toolkit gráfico próprio.

## Status

Projeto em fase inicial (Milestone 0 — fundação sem UI: filesystem,
navegação, `FileEntry`). Ainda não há binário utilizável.

## Filosofia de desenvolvimento

KISS, SOLID quando aplicável, TDD para comportamento determinístico,
componentes pequenos, dependências controladas, lógica separada da UI,
mudanças incrementais. Nenhuma abstração criada apenas por antecipação.

## Licença

MIT — veja [`LICENSE`](LICENSE).
