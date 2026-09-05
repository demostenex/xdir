use std::path::Path;

use crate::app::Action;
use crate::core::navigation::Navigation;
use crate::model::FileEntry;

/// Toolkit-agnostic application state. This is the source of truth; any UI
/// model (a Slint `ModelRc`, or anything else) is only a projection of it.
pub struct AppState {
    navigation: Navigation,
    entries: Vec<FileEntry>,
    selected: Option<usize>,
}

impl AppState {
    pub fn new(navigation: Navigation) -> Self {
        let entries = navigation.entries().unwrap_or_default();
        let selected = initial_selection(&entries);
        AppState {
            navigation,
            entries,
            selected,
        }
    }

    pub fn current_dir(&self) -> &Path {
        self.navigation.current_dir()
    }

    pub fn entries(&self) -> &[FileEntry] {
        &self.entries
    }

    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    pub fn selected_entry(&self) -> Option<&FileEntry> {
        self.selected.and_then(|i| self.entries.get(i))
    }

    /// Applies `action`, the only way any part of this state changes.
    pub fn dispatch(&mut self, action: Action) {
        match action {
            Action::SelectNext => self.select_by(1),
            Action::SelectPrevious => self.select_by(-1),
            Action::SelectIndex(index) => self.select_index(index),
            Action::ActivateSelected => self.activate_selected(),
            Action::ActivateIndex(index) => {
                self.select_index(index);
                self.activate_selected();
            }
            Action::GoParent => self.go_parent(),
        }
    }

    fn select_by(&mut self, delta: i32) {
        if self.entries.is_empty() {
            return;
        }
        let last = self.entries.len() as i32 - 1;
        let current = self.selected.map(|i| i as i32).unwrap_or(-1);
        self.selected = Some((current + delta).clamp(0, last) as usize);
    }

    fn select_index(&mut self, index: usize) {
        if index < self.entries.len() {
            self.selected = Some(index);
        }
    }

    /// Activates the selected entry. Only directories (or symlinks that
    /// resolve to one) cause navigation; activating a regular file is a
    /// deliberate no-op in this milestone (openers arrive later).
    fn activate_selected(&mut self) {
        let Some(target) = self
            .selected_entry()
            .map(|entry| entry.path().to_path_buf())
        else {
            return;
        };
        if self.navigation.navigate_to(&target).is_ok() {
            self.reload();
        }
    }

    fn go_parent(&mut self) {
        if self.navigation.go_to_parent().is_ok() {
            self.reload();
        }
    }

    fn reload(&mut self) {
        self.entries = self.navigation.entries().unwrap_or_default();
        self.selected = initial_selection(&self.entries);
    }
}

fn initial_selection(entries: &[FileEntry]) -> Option<usize> {
    if entries.is_empty() { None } else { Some(0) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::fs;

    fn names(state: &AppState) -> Vec<String> {
        state
            .entries()
            .iter()
            .map(|e| e.name().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn loads_entries_from_the_starting_directory() {
        let dir = TempDir::new();
        fs::write(dir.path().join("a.txt"), b"").unwrap();
        fs::write(dir.path().join("b.txt"), b"").unwrap();

        let state = AppState::new(Navigation::new(dir.path().to_path_buf()).unwrap());

        assert_eq!(names(&state), vec!["a.txt", "b.txt"]);
    }

    #[test]
    fn empty_directory_has_no_selection() {
        let dir = TempDir::new();

        let state = AppState::new(Navigation::new(dir.path().to_path_buf()).unwrap());

        assert_eq!(state.selected(), None);
    }

    #[test]
    fn non_empty_directory_selects_the_first_entry() {
        let dir = TempDir::new();
        fs::write(dir.path().join("a.txt"), b"").unwrap();

        let state = AppState::new(Navigation::new(dir.path().to_path_buf()).unwrap());

        assert_eq!(state.selected(), Some(0));
    }

    #[test]
    fn select_next_does_not_pass_the_last_entry() {
        let dir = TempDir::new();
        fs::write(dir.path().join("a.txt"), b"").unwrap();
        fs::write(dir.path().join("b.txt"), b"").unwrap();
        let mut state = AppState::new(Navigation::new(dir.path().to_path_buf()).unwrap());

        state.dispatch(Action::SelectNext);
        state.dispatch(Action::SelectNext);
        state.dispatch(Action::SelectNext);

        assert_eq!(state.selected(), Some(1));
    }

    #[test]
    fn select_previous_does_not_pass_the_first_entry() {
        let dir = TempDir::new();
        fs::write(dir.path().join("a.txt"), b"").unwrap();
        fs::write(dir.path().join("b.txt"), b"").unwrap();
        let mut state = AppState::new(Navigation::new(dir.path().to_path_buf()).unwrap());

        state.dispatch(Action::SelectPrevious);
        state.dispatch(Action::SelectPrevious);

        assert_eq!(state.selected(), Some(0));
    }

    #[test]
    fn activate_selected_enters_a_directory() {
        let dir = TempDir::new();
        fs::create_dir(dir.path().join("sub")).unwrap();
        let mut state = AppState::new(Navigation::new(dir.path().to_path_buf()).unwrap());

        state.dispatch(Action::ActivateSelected);

        assert_eq!(state.current_dir(), dir.path().join("sub"));
    }

    #[test]
    fn activate_index_selects_then_enters_a_directory() {
        let dir = TempDir::new();
        fs::write(dir.path().join("a_file.txt"), b"").unwrap();
        fs::create_dir(dir.path().join("z_dir")).unwrap();
        let mut state = AppState::new(Navigation::new(dir.path().to_path_buf()).unwrap());
        // sorted: directories first, so "z_dir" is index 0 and "a_file.txt" is index 1
        assert_eq!(names(&state), vec!["z_dir", "a_file.txt"]);

        state.dispatch(Action::ActivateIndex(0));

        assert_eq!(state.current_dir(), dir.path().join("z_dir"));
    }

    #[test]
    fn go_parent_navigates_up_and_reloads_entries() {
        let dir = TempDir::new();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("inner.txt"), b"").unwrap();
        let mut state = AppState::new(Navigation::new(sub).unwrap());
        assert_eq!(names(&state), vec!["inner.txt"]);

        state.dispatch(Action::GoParent);

        assert_eq!(state.current_dir(), dir.path());
        assert_eq!(names(&state), vec!["sub"]);
    }

    #[test]
    fn changing_directory_resets_selection() {
        let dir = TempDir::new();
        fs::create_dir(dir.path().join("sub")).unwrap();
        let mut state = AppState::new(Navigation::new(dir.path().to_path_buf()).unwrap());

        state.dispatch(Action::ActivateSelected);

        assert_eq!(state.selected(), None); // "sub" is empty
    }

    #[test]
    fn invalid_index_does_not_corrupt_state() {
        let dir = TempDir::new();
        fs::write(dir.path().join("a.txt"), b"").unwrap();
        let mut state = AppState::new(Navigation::new(dir.path().to_path_buf()).unwrap());

        state.dispatch(Action::SelectIndex(999));
        assert_eq!(state.selected(), Some(0));

        state.dispatch(Action::ActivateIndex(999));
        assert_eq!(state.selected(), Some(0));
        assert_eq!(state.current_dir(), dir.path());
    }
}
