//! A tab's history: every picture it showed, for the Back and Forward keys
//! and mouse buttons, as a browser's. No other UI.

use std::path::{Path, PathBuf};

/// Entries kept each way: a few KB a tab at most.
const KEEP: usize = 50;

#[derive(Default, Debug)]
pub struct History {
    back: Vec<PathBuf>,
    forward: Vec<PathBuf>,
}

fn push(stack: &mut Vec<PathBuf>, path: PathBuf) {
    if stack.last() != Some(&path) {
        stack.push(path);
        if stack.len() > KEEP {
            stack.remove(0);
        }
    }
}

impl History {
    /// The tab moved on from `left` by any way but Back and Forward: it is
    /// the way back, and the way forward is gone.
    pub fn visited(&mut self, left: PathBuf) {
        push(&mut self.back, left);
        self.forward.clear();
    }

    /// Back from `here` (the tab's picture): where to, if anywhere. `here`
    /// becomes the way forward.
    pub fn back(&mut self, here: &Path) -> Option<PathBuf> {
        let to = pop_other(&mut self.back, here)?;
        push(&mut self.forward, here.to_path_buf());
        Some(to)
    }

    pub fn forward(&mut self, here: &Path) -> Option<PathBuf> {
        let to = pop_other(&mut self.forward, here)?;
        push(&mut self.back, here.to_path_buf());
        Some(to)
    }
}

/// The latest entry that isn't `here` (nor a file since deleted).
fn pop_other(stack: &mut Vec<PathBuf>, here: &Path) -> Option<PathBuf> {
    while let Some(path) = stack.pop() {
        if path != here && path.exists() {
            return Some(path);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goes_back_and_forward_like_a_browser() {
        let dir = std::env::temp_dir().join("flick-history-test");
        std::fs::create_dir_all(&dir).unwrap();
        let [a, b, c, d] = ["a", "b", "c", "d"].map(|n| {
            let path = dir.join(n);
            std::fs::write(&path, n).unwrap();
            path
        });
        let mut history = History::default();
        history.visited(a.clone());
        history.visited(b.clone());
        // On c: back to b, back to a, forward to b, forward to c.
        assert_eq!(history.back(&c), Some(b.clone()));
        assert_eq!(history.back(&b), Some(a.clone()));
        assert_eq!(history.back(&a), None, "nothing before a");
        assert_eq!(history.forward(&a), Some(b.clone()));
        assert_eq!(history.forward(&b), Some(c.clone()));
        assert_eq!(history.forward(&c), None);
        // Back to b, then elsewhere by hand: the way forward is gone.
        assert_eq!(history.back(&c), Some(b.clone()));
        history.visited(b.clone());
        assert_eq!(history.forward(&d), None);
        assert_eq!(history.back(&d), Some(b.clone()));
        // A deleted file is skipped.
        std::fs::remove_file(&a).unwrap();
        assert_eq!(history.back(&b), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn keeps_a_bounded_list() {
        let mut history = History::default();
        for i in 0..200 {
            history.visited(PathBuf::from(format!("{i}")));
        }
        assert_eq!(history.back.len(), KEEP);
    }
}
