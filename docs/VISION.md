# xdir — Visão e Arquitetura

## Visão

**xdir** é um gerenciador gráfico de arquivos para Linux/X11, projetado para
usuários que gostam da eficiência de ferramentas como Yazi, Ranger e editores
modais, mas não querem abrir mão das vantagens reais de uma interface
gráfica.

A proposta não é criar um Yazi dentro de uma janela.

A proposta é perguntar:

> Como seria um gerenciador de arquivos gráfico se ele tivesse sido projetado
> a partir das ideias de navegação, contexto e eficiência dos file managers
> de terminal, em vez de copiar Explorer, Nautilus, Dolphin ou Thunar?

O xdir deve funcionar sozinho.

Quando executado em um ambiente com **xomposite** e **xbar**, poderá utilizar
integrações adicionais através de IPC e hints explícitos, mas nenhum desses
componentes deve ser requisito para que o gerenciador funcione.

---

## Princípios

### 1. GUI de verdade

O xdir não é uma TUI renderizada dentro de uma janela.

Ele deve suportar naturalmente recursos gráficos como:

- thumbnails;
- previews de imagens;
- previews de PDF;
- drag-and-drop;
- clipboard;
- menus contextuais;
- seleção com mouse;
- scroll;
- ícones;
- fontes proporcionais ou monoespaçadas conforme contexto;
- integração com MIME types;
- notificações visuais;
- previews ricos.

O teclado, porém, deve ser tão poderoso quanto o mouse.

### 2. Keyboard-first, não keyboard-only

Toda operação importante deve poder ser executada pelo teclado.

Exemplos:

- `j` / `k`: mover seleção;
- `h`: diretório pai;
- `l` ou `Enter`: entrar/abrir;
- `Space`: selecionar;
- `/`: buscar;
- `.`: alternar arquivos ocultos;
- `yy`: copiar;
- `dd`: cortar;
- `p`: colar;
- `r`: renomear;
- `Delete`: apagar;
- `Ctrl+h/j/k/l`: navegar entre painéis;
- atalhos configuráveis.

Mas a mesma aplicação deve funcionar perfeitamente com clique, duplo clique,
Ctrl+click, Shift+click, drag-and-drop, contexto com botão direito e scroll.

Mouse e teclado devem acessar o mesmo conjunto de ações internas.

---

## Layout principal

```text
┌──────────────────┬────────────────────────────┬──────────────────────┐
│ PARENT           │ CURRENT                    │ PREVIEW              │
│                  │                            │                      │
│ src/             │ graphics/                  │ renderer.rs          │
│ tests/           │ x11/                       │                      │
│ docs/            │ main.rs                    │ pub struct ...       │
│                  │ renderer.rs  ←             │                      │
│                  │                            │                      │
└──────────────────┴────────────────────────────┴──────────────────────┘
```

Filosofia: `contexto → conteúdo atual → conteúdo selecionado`.

Essa visualização deve ser tratada como uma das características centrais do
projeto.

---

## Tiling interno

O xdir deve possuir painéis divisíveis — não um window manager embutido, mas
a possibilidade de dividir o workspace em múltiplas visualizações de
diretórios, cada painel navegando independentemente.

O layout pode ser representado internamente como uma árvore:

```text
Split::Horizontal
├── Pane
└── Split::Vertical
    ├── Pane
    └── Pane
```

Inicialmente apenas: split vertical, split horizontal, fechar painel, mudar
foco, trocar diretório independentemente em cada painel. Não implementar
layouts automáticos complexos no começo.

---

## Visualizações

- **Columns** (padrão): `parent → current → preview`.
- **List**: lista tradicional de nomes.
- **Grid** (posterior): voltada para imagens/vídeos.

A visualização pertence ao painel — dois painéis podem usar modos diferentes
simultaneamente.

---

## Preview

Preview é parte central do xdir, não um complemento. Interface conceitual:

```rust
trait PreviewProvider {
    fn supports(&self, file: &FileEntry) -> bool;

    fn preview(
        &self,
        file: &FileEntry,
        context: PreviewContext,
    ) -> Result<Preview>;
}
```

Providers futuros: imagem, texto, código (syntax highlighting), PDF, vídeo
(frame + metadata), áudio (metadata), archive (conteúdo), diretório
(resumo), planilha (preview tabular). Não implementar tudo inicialmente — a
arquitetura deve permitir plugins/providers futuros.

---

## Arquitetura conceitual

```text
xdir
│
├── core
│   ├── filesystem
│   ├── navigation
│   ├── selection
│   ├── operations
│   └── history
│
├── workspace
│   ├── pane
│   ├── split
│   └── focus
│
├── preview
│   ├── text
│   ├── image
│   └── providers
│
├── openers
├── mime
├── ui
├── input
│   ├── keyboard
│   └── mouse
├── ipc
│
└── integrations
    ├── xbar
    └── xomposite
```

A lógica de filesystem não deve depender da UI. A UI não deve executar
diretamente operações destrutivas no filesystem.

---

## Operações de arquivos

```text
UI → Command → FileOperation → Filesystem
```

```rust
enum FileOperation {
    Copy { sources: Vec<PathBuf>, destination: PathBuf },
    Move { sources: Vec<PathBuf>, destination: PathBuf },
    Rename { source: PathBuf, destination: PathBuf },
    Trash { sources: Vec<PathBuf> },
}
```

Isso permite futuramente: progresso, cancelamento, undo, logs, operações
assíncronas, integração com xbar. Não colocar `std::fs::copy()` diretamente
em handlers da UI.

---

## Openers

O usuário escolhe exatamente como cada arquivo é aberto:

```toml
[openers]

text = [
    { command = "nvim", args = ["{file}"], terminal = true }
]

pdf = [
    { command = "zathura", args = ["{file}"] }
]

video = [
    { command = "mpv", args = ["{file}"] }
]

spreadsheet = [
    { command = "excel-tui", args = ["{file}"], terminal = true }
]
```

O sistema poderá compreender futuramente: MIME, extensão, glob, regra
explícita, prioridade. Não depender exclusivamente do banco MIME do desktop.

---

## Integração com xbar (opcional)

O xdir pode publicar eventos via Unix socket:

```text
xdir.operation.started
xdir.operation.progress
xdir.operation.finished
xdir.directory.changed
xdir.selection.changed
```

Payload de exemplo:

```json
{
  "event": "operation.progress",
  "type": "copy",
  "current": 734003200,
  "total": 1073741824,
  "files_done": 17,
  "files_total": 32
}
```

O xdir jamais deve depender da xbar para mostrar ou concluir uma operação.

---

## Integração com xomposite (opcional)

O xdir pode declarar hints para efeitos específicos: blur permitido em
regiões determinadas, transparência, shadow especial, popup/overlay, preview
fullscreen.

Regra: **capability não implica utilização.** O xdir deve solicitar
explicitamente qualquer comportamento especial. O xomposite jamais deve
assumir que uma janela do xdir deseja blur simplesmente porque suporta
transparência.

---

## Tecnologia

- **Linguagem:** Rust.
- **Prioridades:** segurança, previsibilidade, desempenho, baixo consumo,
  integração nativa com Linux, fácil comunicação com xomposite/xbar.
- **Não utilizar:** Electron, Chromium, WebView como UI principal.
- Toolkit gráfico a decidir após spike técnico (candidatos: GTK4, iced, ou
  outro toolkit Rust suficientemente maduro). Não criar um toolkit gráfico
  próprio durante o desenvolvimento inicial.

---

## Filosofia de desenvolvimento

KISS; SOLID quando aplicável; TDD para comportamento determinístico;
componentes pequenos; dependências controladas; lógica separada da UI;
mudanças incrementais; nenhuma abstração criada apenas por antecipação. Não
implementar feature porque "talvez seja necessária um dia".

---

## Definição inicial do MVP (0.1)

O xdir 0.1 deve conseguir:

- abrir uma janela; receber um path; navegar no filesystem;
- mostrar parent/current/preview;
- funcionar completamente com teclado e com mouse;
- abrir arquivos através de openers configuráveis;
- visualizar texto e imagens;
- selecionar, copiar, mover, renomear, mandar para trash;
- dividir a interface vertical/horizontalmente;
- navegar independentemente em cada painel.

Não precisa ainda: xbar; xomposite; rede; plugins; vídeo; archive browsing;
terminal integrado; Git; tabs.

## Regra de ouro

Sempre que surgir uma nova feature, perguntar:

> Isso torna a navegação e manipulação de arquivos mais rápida ou estamos
> apenas transformando o xdir em outro desktop environment?

Se a resposta for a segunda opção, não implementar.
