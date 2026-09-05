//! xdir: núcleo de filesystem/navegação (Milestone 0) mais uma primeira
//! janela gráfica em Slint (Milestone 1B).
//!
//! Arquitetura em camadas, de baixo para cima:
//!
//! ```text
//! core/model   — filesystem e navegação; não conhece nenhum toolkit de UI.
//! app          — AppState/Action; toolkit-agnostic, é a fonte da verdade.
//! ui           — única camada que importa `slint`; projeta AppState na tela.
//! ```
//!
//! `core` e `model` nunca importam `slint`. `ui` nunca chama `core`/`model`
//! diretamente — sempre through `app::AppState::dispatch`.

pub mod app;
pub mod core;
pub mod model;
pub mod ui;

/// Utilitário exclusivo de testes: diretório temporário isolado e único por
/// chamada, sem tocar `$HOME` ou usar nomes fixos em `/tmp`.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    pub(crate) struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        pub(crate) fn new() -> Self {
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock before UNIX epoch")
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("xdir-test-{}-{unique}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&path).expect("failed to create isolated temp dir for test");
            TempDir { path }
        }

        pub(crate) fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}
