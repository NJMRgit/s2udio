use crate::ui::dirstack::{Dir, DirStack, DirStackItem, Path, ScrollingState};
pub struct Walk<'a, T, S>
where
    T: std::fmt::Debug + DirStackItem + Clone + Send,
    S: ScrollingState + std::fmt::Debug + Default,
{
    stack: &'a DirStack<T, S>,
    dir: Option<&'a Dir<T, S>>,
    item: Option<&'a T>,
    walker: Option<Box<Walk<'a, T, S>>>,
    path: Path,
    idx: usize,
}
pub trait WalkDirStackItem<'a, T, S>
where
    T: std::fmt::Debug + DirStackItem + Clone + Send,
    S: ScrollingState + std::fmt::Debug + Default,
{
    fn walk(&'a self, stack: &'a DirStack<T, S>, path: Path) -> Walk<'a, T, S>;
}
impl<'a, T, S> WalkDirStackItem<'a, T, S> for Dir<T, S>
where
    T: std::fmt::Debug + DirStackItem + Clone + Send,
    S: ScrollingState + std::fmt::Debug + Default,
{
    fn walk(&'a self, stack: &'a DirStack<T, S>, path: Path) -> Walk<'a, T, S> {
        let dir = stack.get(&path);
        Walk {
            stack,
            dir,
            item: None,
            walker: None,
            path,
            idx: 0,
        }
    }
}
impl<'a, T, S> WalkDirStackItem<'a, T, S> for T
where
    T: std::fmt::Debug + DirStackItem + Clone + Send,
    S: ScrollingState + std::fmt::Debug + Default,
{
    fn walk(&'a self, stack: &'a DirStack<T, S>, path: Path) -> Walk<'a, T, S> {
        if self.is_file() {
            Walk {
                stack,
                dir: None,
                item: Some(self),
                walker: None,
                path,
                idx: 0,
            }
        } else {
            let path = path.join(self.as_path());
            let dir = stack.get(&path);
            Walk {
                stack,
                dir,
                item: None,
                walker: None,
                path,
                idx: 0,
            }
        }
    }
}
impl<'a, T, S> Iterator for Walk<'a, T, S>
where
    T: std::fmt::Debug + DirStackItem + Clone + Send,
    S: ScrollingState + std::fmt::Debug + Default,
{
    type Item = &'a T;
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(item) = self.item.take() {
            return Some(item);
        }
        if let Some(walker) = &mut self.walker {
            if let Some(item) = walker.next() {
                return Some(item);
            }
            self.walker = None;
        }
        let dir = self.dir?;
        if let Some(item) = dir.items.get(self.idx) {
            self.idx += 1;
            if item.is_file() {
                return Some(item);
            }
            let subpath = self.path.join(item.as_path());
            let subdir = self.stack.get(&subpath);
            self.walker = subdir
                .map(|subdir| Box::new(subdir.walk(self.stack, subpath)));
            return self.next();
        }
        return None;
    }
}
