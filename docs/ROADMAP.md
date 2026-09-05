# xdir — Roadmap

Desenvolvimento incremental por milestones. Nenhuma milestone deve antecipar
trabalho de uma milestone futura.

## Milestone 0 — Fundação

Núcleo sem interface gráfica.

- projeto Rust; organização dos módulos;
- `FileEntry`; leitura de diretórios; ordenação; navegação;
- diretório atual; diretório pai; arquivos ocultos;
- testes.

Critério de conclusão: `cargo test` verde e filesystem navegável através de
testes/API.

Não implementar ainda: toolkit gráfico, X11, IPC, thumbnails, async runtime
sem necessidade demonstrada, sistema de plugins, operações destrutivas,
integração com xbar/xomposite.

## Milestone 1 — Primeira janela

Escolha do toolkit; janela principal; listagem de arquivos; navegação;
seleção; scroll; entrada em diretórios.

Ainda não: preview, thumbnails, tiling, drag-and-drop.

Critério: `xdir ~/Projetos` abre e permite navegar pela árvore com mouse e
teclado.

## Milestone 2 — Keyboard-first

`j`, `k`, `h`, `l`, `Enter`, `/`, `.`, `Space`. Sistema centralizado de
actions/keybindings — evitar handlers espalhados pela UI.

```text
Input → Action → Application State → UI
```

## Milestone 3 — Layout assinatura

`parent | current | preview`, com preview simples: resumo/listagem para
diretório, primeiras linhas para texto, metadata para tipos desconhecidos.
Estabelece a identidade visual inicial do xdir.

## Milestone 4 — Preview Providers

Interface estável de preview. Primeiros providers: texto, código, imagem.
Depois: PDF. Não implementar vídeo, áudio, archive ou spreadsheet ainda.

## Milestone 5 — Openers

Configuração TOML de openers. Suportar MIME, extensão, fallback do sistema.
Abrir arquivos não deve bloquear a UI desnecessariamente.

## Milestone 6 — Seleção e clipboard interno

Seleção múltipla; copy; cut; paste; rename; trash — via comandos internos.
Testes extensivos com diretórios temporários. Nenhum teste deve tocar
`$HOME`.

## Milestone 7 — File Operations Engine

Separar completamente operações da UI: fila, progresso, cancelamento,
tratamento de conflito, overwrite, rename-on-conflict, skip.

## Milestone 8 — Tiling

```rust
enum WorkspaceNode {
    Pane(PaneId),
    Split {
        axis: Axis,
        ratio: f32,
        first: Box<WorkspaceNode>,
        second: Box<WorkspaceNode>,
    },
}
```

Split vertical/horizontal; fechar painel; trocar foco; resize; cada painel
com diretório independente. Sem tabs inicialmente.

## Milestone 9 — Drag-and-drop

Dentro do xdir; entre painéis; para/de outras aplicações. Deve passar pelo
mesmo File Operations Engine — sem implementação paralela de copy/move.

## Milestone 10 — Thumbnails

Cache para imagens e PDF (vídeo depois). Geração fora da thread principal;
limite de cache; invalidação; cancelamento quando item deixa o viewport.

## Milestone 11 — IPC

Protocolo local via Unix domain socket:
`directory.changed`, `selection.changed`, `operation.started`,
`operation.progress`, `operation.finished`. Primeiro consumidor: xbar — mas
o protocolo não deve conhecer xbar diretamente.

## Milestone 12 — Integração xbar

xbar é apenas um consumidor do IPC (ex.: caminho atual, progresso de
operação).

## Milestone 13 — Integração xomposite

Somente após o protocolo do xomposite estar suficientemente estável: window
hints, preview overlay, blur opt-in, fullscreen preview, animações
específicas. Nunca criar dependência obrigatória.

---

## Depois do MVP

Tabs; bookmarks; fuzzy finder; command palette; bulk rename; archive
browsing; mount management; GVFS; SMB/SFTP; Git status; terminal integrado;
custom previewers; custom commands; session restore; bookmarks
compartilháveis; actions configuráveis; plugins. Nenhuma delas pertence ao
MVP.
