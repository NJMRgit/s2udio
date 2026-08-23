use std::collections::HashMap;
use super::{DirStackItem, dir::Dir, state::DirState};
use crate::ui::dirstack::{ScrollingState, path::Path};
#[derive(Debug)]
pub struct DirStack<T, S>
where
    T: std::fmt::Debug + DirStackItem + Clone + Send,
    S: ScrollingState + std::fmt::Debug + Default,
{
    path: Path,
    dirs: HashMap<Path, Dir<T, S>>,
    empty: Dir<T, S>,
}
impl<T, S> Default for DirStack<T, S>
where
    T: std::fmt::Debug + DirStackItem + Clone + Send,
    S: ScrollingState + std::fmt::Debug + Default,
{
    fn default() -> Self {
        DirStack::new(Vec::default())
    }
}
#[allow(dead_code)]
impl<T, S> DirStack<T, S>
where
    T: std::fmt::Debug + DirStackItem + Clone + Send,
    S: ScrollingState + std::fmt::Debug + Default,
{
    pub fn new(root: Vec<T>) -> Self {
        let mut result = Self {
            dirs: HashMap::new(),
            path: Path::new(),
            empty: Dir::new(Vec::new()),
        };
        result.dirs.insert(result.path.clone(), Dir::new(root));
        result
    }
    pub fn len(&self) -> usize {
        self.dirs.len()
    }
    pub fn get(&self, path: &Path) -> Option<&Dir<T, S>> {
        self.dirs.get(path)
    }
    pub fn current(&self) -> &Dir<T, S> {
        self.dirs.get(&self.path).unwrap_or(&self.empty)
    }
    pub fn current_mut(&mut self) -> &mut Dir<T, S> {
        self.dirs.get_mut(&self.path).unwrap_or(&mut self.empty)
    }
    /// The root directory ("" path) of the stack — the playlists/browser
    /// list shown in the left pane.
    pub fn root(&self) -> &Dir<T, S> {
        self.dirs.get(&Path::new()).unwrap_or(&self.empty)
    }
    pub fn root_mut(&mut self) -> &mut Dir<T, S> {
        self.dirs.get_mut(&Path::new()).unwrap_or(&mut self.empty)
    }
    pub fn previous(&self) -> Option<&Dir<T, S>> {
        if self.path.is_empty() {
            None
        } else {
            let mut path = self.path.clone();
            path.pop();
            self.dirs.get(&path)
        }
    }
    pub fn previous_mut(&mut self) -> Option<&mut Dir<T, S>> {
        if self.path.is_empty() {
            None
        } else {
            let mut path = self.path.clone();
            path.pop();
            self.dirs.get_mut(&path)
        }
    }
    pub fn next(&self) -> Option<&Dir<T, S>> {
        self.next_path().and_then(|path| self.dirs.get(&path))
    }
    pub fn next_mut(&mut self) -> Option<&mut Dir<T, S>> {
        self.next_path().and_then(|path| self.dirs.get_mut(&path))
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn next_path(&self) -> Option<Path> {
        self.current()
            .selected()
            .map(DirStackItem::as_path)
            .map(|current| self.path.join(current))
    }
    pub fn next_dir_items(&self) -> Option<&Vec<T>> {
        self.next_path().and_then(|path| self.dirs.get(&path).map(|d| &d.items))
    }
    pub fn insert(&mut self, path: Path, items: Vec<T>) {
        let mut new_state = DirState::default();
        if !items.is_empty() {
            new_state.select(Some(0), 0);
        }
        new_state.set_content_len(Some(items.len()));
        self.dirs.insert(path, Dir::new_with_state(items, new_state));
    }
    pub fn enter(&mut self) {
        if let Some(next_path) = self.next_path() {
            self.path = next_path;
            if !self.dirs.contains_key(&self.path) {
                self.dirs.insert(self.path.clone(), Dir::default());
            }
        } else {
            log::error!(
                stack:? = self; "Cannot enter because next path is not available"
            );
        }
    }
    pub fn leave(&mut self) -> bool {
        if self.path.is_empty() {
            false
        } else {
            self.path.pop();
            true
        }
    }
}
