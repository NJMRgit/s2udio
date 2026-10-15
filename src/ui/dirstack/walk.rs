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




#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod test {
    use itertools::Itertools;

    use crate::{
        mpd::commands::Song,
        ui::{
            dir_or_song::DirOrSong,
            dirstack::{DirStack, DirStackItem, ListState, WalkDirStackItem},
        },
    };

    fn song(name: &str) -> DirOrSong {
        DirOrSong::Song(Song { file: name.to_owned(), ..Default::default() })
    }










    #[test]
    fn walk_called_on_file_yields_only_that_file() {
        let mut stack = DirStack::<DirOrSong, ListState>::new(vec![
            DirOrSong::name_only("dirA".to_owned()),
            song("song1"),
            DirOrSong::name_only("dirB".to_owned()),
        ]);
        stack.insert(["dirA"].into(), vec![song("songA1"), song("songA2")]);
        stack.insert(["dirB"].into(), vec![song("songB1")]);
        stack.current_mut().select_idx(0, 0); // select dirA
        stack.enter();
        stack.current_mut().select_idx(1, 0); //select songA2

        let result =
            stack.current().selected().unwrap().walk(&stack, stack.path().to_owned()).collect_vec();

        assert_eq!(result, vec![&song("songA2")]);
    }

    #[test]
    fn walk_called_on_empty_dir_yields_no_songs() {
        let mut stack = DirStack::<DirOrSong, ListState>::new(vec![
            DirOrSong::name_only("dirA".to_owned()),
            song("song1"),
            DirOrSong::name_only("dirB".to_owned()),
        ]);

        stack.insert(["dirA"].into(), vec![song("songA1"), song("songA2")]);
        stack.insert(["dirB"].into(), vec![]);
        stack.current_mut().select_idx(2, 0); // select dirB
        assert_eq!(stack.current().selected().unwrap().as_path(), "dirB");

        let result =
            stack.current().selected().unwrap().walk(&stack, stack.path().to_owned()).collect_vec();

        assert!(result.is_empty());
    }

    #[test]
    fn walk_called_on_dir_yields_songs_in_that_dir() {
        let mut stack = DirStack::<DirOrSong, ListState>::new(vec![
            DirOrSong::name_only("dirA".to_owned()),
            song("song1"),
            DirOrSong::name_only("dirB".to_owned()),
        ]);
        stack.insert(["dirA"].into(), vec![song("songA1"), song("songA2")]);
        stack.insert(["dirB"].into(), vec![song("songB1")]);
        stack.current_mut().select_idx(0, 0); // select dirA
        assert_eq!(stack.current().selected().unwrap().as_path(), "dirA");

        let test = stack.current().selected().unwrap();
        let result = test.walk(&stack, stack.path().to_owned()).collect_vec();

        assert_eq!(result, vec![&song("songA1"), &song("songA2")]);
    }

    #[test]
    fn walk_called_on_dir_yields_songs_in_that_dir_and_its_subdirs() {
        let mut stack = DirStack::<DirOrSong, ListState>::new(vec![
            DirOrSong::name_only("dirA".to_owned()),
            song("song1"),
            DirOrSong::name_only("dirB".to_owned()),
        ]);
        stack.insert(
            ["dirA"].into(),
            vec![song("songA1"), DirOrSong::name_only("dirA1".to_owned()), song("songA2")],
        );
        stack.insert(["dirA", "dirA1"].into(), vec![song("songA1a"), song("songA1b")]);
        stack.insert(["dirB"].into(), vec![song("songB1")]);
        stack.current_mut().select_idx(0, 0); // select dirA
        assert_eq!(stack.current().selected().unwrap().as_path(), "dirA");

        let test = stack.current().selected().unwrap();
        let result = test.walk(&stack, stack.path().to_owned()).collect_vec();

        assert_eq!(
            result,
            vec![&song("songA1"), &song("songA1a"), &song("songA1b"), &song("songA2"),]
        );
    }
}
