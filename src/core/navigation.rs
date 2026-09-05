use std::ffi::OsStr;
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::core::filesystem;
use crate::model::FileEntry;

/// Estado de navegação do filesystem, independente de qualquer UI.
///
/// Não canonicaliza o diretório atual: o caminho lógico usado para chegar a
/// um diretório (inclusive através de symlinks) é preservado, em vez de ser
/// resolvido para o alvo real.
#[derive(Debug)]
pub struct Navigation {
    current_dir: PathBuf,
    show_hidden: bool,
}

impl Navigation {
    /// Inicia a navegação em `start_dir`.
    ///
    /// `start_dir` é tornado absoluto (via [`std::path::absolute`], que não
    /// acessa o filesystem nem resolve symlinks) e normalizado logicamente
    /// (sem componentes `.`/`..`) antes de ser validado e armazenado, para
    /// que `current_dir` nunca fique implicitamente dependente do current
    /// working directory do processo, nem carregue `..` pendente. Falha se
    /// o resultado não for um diretório existente.
    pub fn new(start_dir: PathBuf) -> io::Result<Self> {
        let start_dir = normalize_logical(&std::path::absolute(start_dir)?);
        ensure_is_directory(&start_dir)?;

        Ok(Navigation {
            current_dir: start_dir,
            show_hidden: false,
        })
    }

    pub fn current_dir(&self) -> &Path {
        &self.current_dir
    }

    pub fn parent_dir(&self) -> Option<&Path> {
        self.current_dir.parent()
    }

    pub fn show_hidden(&self) -> bool {
        self.show_hidden
    }

    pub fn set_show_hidden(&mut self, show_hidden: bool) {
        self.show_hidden = show_hidden;
    }

    /// Lista as entradas do diretório atual, respeitando `show_hidden`.
    pub fn entries(&self) -> io::Result<Vec<FileEntry>> {
        filesystem::read_directory(&self.current_dir, self.show_hidden)
    }

    /// Navega para `target`. Um `target` relativo é resolvido em relação ao
    /// diretório atual da navegação — nunca ao diretório de trabalho do
    /// processo. O resultado é normalizado logicamente (`.`/`..` resolvidos
    /// sem tocar o filesystem, sem seguir symlinks), então `..` a partir de
    /// um diretório alcançado via symlink volta ao pai lógico, não ao pai do
    /// alvo físico do symlink. Se o alvo não for um diretório válido, o
    /// estado atual permanece inalterado.
    pub fn navigate_to(&mut self, target: &Path) -> io::Result<()> {
        let resolved = normalize_logical(&self.resolve(target));
        ensure_is_directory(&resolved)?;
        self.current_dir = resolved;
        Ok(())
    }

    /// Entra no subdiretório `name`, filho direto do diretório atual.
    pub fn enter(&mut self, name: &OsStr) -> io::Result<()> {
        self.navigate_to(Path::new(name))
    }

    /// Volta ao diretório pai. Falha sem alterar o estado se o diretório
    /// atual não tiver pai ou se o pai não for mais um diretório válido.
    pub fn go_to_parent(&mut self) -> io::Result<()> {
        let parent = self
            .current_dir
            .parent()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "current directory has no parent")
            })?
            .to_path_buf();

        ensure_is_directory(&parent)?;
        self.current_dir = parent;
        Ok(())
    }

    fn resolve(&self, target: &Path) -> PathBuf {
        if target.is_absolute() {
            target.to_path_buf()
        } else {
            self.current_dir.join(target)
        }
    }
}

/// Normaliza `path` puramente lexicalmente: remove componentes `.`,
/// resolve `..` eliminando o componente anterior (sem ultrapassar a raiz),
/// e nunca acessa o filesystem nem resolve symlinks.
fn normalize_logical(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match result.components().next_back() {
                Some(Component::Normal(_)) => {
                    result.pop();
                }
                Some(Component::RootDir) | Some(Component::Prefix(_)) => {}
                _ => result.push(component),
            },
            other => result.push(other),
        }
    }

    result
}

fn ensure_is_directory(path: &Path) -> io::Result<()> {
    let metadata = std::fs::metadata(path)?;
    if metadata.is_dir() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a directory", path.display()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::fs;

    #[test]
    fn starts_at_given_directory() {
        let dir = TempDir::new();

        let navigation = Navigation::new(dir.path().to_path_buf()).unwrap();

        assert_eq!(navigation.current_dir(), dir.path());
    }

    #[test]
    fn reports_parent_directory() {
        let dir = TempDir::new();
        let child = dir.path().join("child");
        fs::create_dir(&child).unwrap();

        let navigation = Navigation::new(child).unwrap();

        assert_eq!(navigation.parent_dir(), Some(dir.path()));
    }

    #[test]
    fn enters_subdirectory() {
        let dir = TempDir::new();
        fs::create_dir(dir.path().join("sub")).unwrap();
        let mut navigation = Navigation::new(dir.path().to_path_buf()).unwrap();

        navigation.enter(OsStr::new("sub")).unwrap();

        assert_eq!(navigation.current_dir(), dir.path().join("sub"));
    }

    #[test]
    fn returns_to_parent_directory() {
        let dir = TempDir::new();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        let mut navigation = Navigation::new(sub).unwrap();

        navigation.go_to_parent().unwrap();

        assert_eq!(navigation.current_dir(), dir.path());
    }

    #[test]
    fn relative_paths_resolve_against_current_dir_not_process_cwd() {
        let dir = TempDir::new();
        fs::create_dir(dir.path().join("foo")).unwrap();
        let mut navigation = Navigation::new(dir.path().to_path_buf()).unwrap();

        navigation.navigate_to(Path::new("foo")).unwrap();

        assert_eq!(navigation.current_dir(), dir.path().join("foo"));
    }

    #[test]
    fn show_hidden_can_be_queried_and_changed() {
        let dir = TempDir::new();
        let mut navigation = Navigation::new(dir.path().to_path_buf()).unwrap();

        assert!(!navigation.show_hidden());

        navigation.set_show_hidden(true);

        assert!(navigation.show_hidden());
    }

    #[test]
    fn navigating_into_a_regular_file_fails() {
        let dir = TempDir::new();
        fs::write(dir.path().join("file.txt"), b"").unwrap();
        let mut navigation = Navigation::new(dir.path().to_path_buf()).unwrap();

        let result = navigation.navigate_to(Path::new("file.txt"));

        assert!(result.is_err());
    }

    #[test]
    fn failed_navigation_does_not_change_current_dir() {
        let dir = TempDir::new();
        let mut navigation = Navigation::new(dir.path().to_path_buf()).unwrap();

        let result = navigation.navigate_to(Path::new("does-not-exist"));

        assert!(result.is_err());
        assert_eq!(navigation.current_dir(), dir.path());
    }

    #[test]
    fn relative_start_dir_is_made_absolute() {
        // "." é relativo ao cwd do processo (não alterado por este teste),
        // logo o esperado é comparado contra `env::current_dir()` em vez de
        // mexer no cwd global — o que quebraria testes rodando em paralelo.
        let expected = std::env::current_dir().unwrap();

        let navigation = Navigation::new(PathBuf::from(".")).unwrap();

        assert!(navigation.current_dir().is_absolute());
        assert_eq!(navigation.current_dir(), expected);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_entries_keep_symlink_kind_even_pointing_to_a_directory() {
        use crate::model::EntryKind;
        use std::os::unix::fs::symlink;

        let dir = TempDir::new();
        let real = dir.path().join("real");
        let link = dir.path().join("link");
        fs::create_dir(&real).unwrap();
        symlink(&real, &link).unwrap();

        let navigation = Navigation::new(dir.path().to_path_buf()).unwrap();
        let entries = navigation.entries().unwrap();

        let link_entry = entries
            .iter()
            .find(|entry| entry.name() == OsStr::new("link"))
            .expect("symlink entry should be listed");
        assert_eq!(link_entry.kind(), EntryKind::Symlink);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_file_names_survive_a_directory_read() {
        use std::os::unix::ffi::OsStrExt;

        let dir = TempDir::new();
        // 0x66, 0x80 ("f" seguido de um byte contínuo solto): sequência
        // inválida em UTF-8, mas um nome de arquivo Unix perfeitamente
        // válido.
        let raw_name = OsStr::from_bytes(&[0x66, 0x80, 0x6f]).to_os_string();
        assert!(
            raw_name.to_str().is_none(),
            "nome deveria ser inválido em UTF-8"
        );
        fs::write(dir.path().join(&raw_name), b"").unwrap();

        let navigation = Navigation::new(dir.path().to_path_buf()).unwrap();
        let entries = navigation.entries().unwrap();

        assert!(
            entries
                .iter()
                .any(|entry| entry.name() == raw_name.as_os_str()),
            "entrada com nome não-UTF-8 deveria ser preservada intacta"
        );
    }

    #[cfg(unix)]
    #[test]
    fn navigation_preserves_the_logical_path_through_a_symlink() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new();
        let real = dir.path().join("real");
        let link = dir.path().join("link");
        fs::create_dir(&real).unwrap();
        symlink(&real, &link).unwrap();

        let mut navigation = Navigation::new(link.clone()).unwrap();

        assert_eq!(navigation.current_dir(), link);
        assert_ne!(navigation.current_dir(), real);

        navigation.go_to_parent().unwrap();

        assert_eq!(navigation.current_dir(), dir.path());
    }

    #[test]
    fn navigating_to_parent_dir_component_produces_a_logically_normalized_path() {
        let dir = TempDir::new();
        let a = dir.path().join("a");
        let b = a.join("b");
        fs::create_dir_all(&b).unwrap();
        let mut navigation = Navigation::new(b).unwrap();

        navigation.navigate_to(Path::new("..")).unwrap();

        assert_eq!(navigation.current_dir(), a);
        assert!(
            !navigation
                .current_dir()
                .components()
                .any(|component| component == Component::ParentDir),
            "current_dir não deveria conter componentes `..`: {:?}",
            navigation.current_dir()
        );
    }

    #[test]
    fn relative_target_with_parent_dir_component_reaches_a_sibling_directory() {
        let dir = TempDir::new();
        let a = dir.path().join("a");
        let b = a.join("b");
        let c = dir.path().join("c");
        fs::create_dir_all(&b).unwrap();
        fs::create_dir_all(&c).unwrap();
        let mut navigation = Navigation::new(b).unwrap();

        navigation.navigate_to(Path::new("../../c")).unwrap();

        assert_eq!(navigation.current_dir(), c);
    }

    #[cfg(unix)]
    #[test]
    fn parent_dir_through_a_symlink_uses_the_logical_path_not_the_physical_target() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new();
        let real = dir.path().join("real");
        let link = dir.path().join("link");
        fs::create_dir(&real).unwrap();
        symlink(&real, &link).unwrap();

        let mut navigation = Navigation::new(link).unwrap();

        navigation.navigate_to(Path::new("..")).unwrap();

        assert_eq!(navigation.current_dir(), dir.path());
    }

    #[test]
    fn initial_dir_with_parent_dir_component_is_normalized() {
        let dir = TempDir::new();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        fs::create_dir(&a).unwrap();
        fs::create_dir(&b).unwrap();

        let navigation = Navigation::new(a.join("..").join("b")).unwrap();

        assert_eq!(navigation.current_dir(), b);
        assert!(
            !navigation
                .current_dir()
                .components()
                .any(|component| component == Component::ParentDir)
        );
    }
}
