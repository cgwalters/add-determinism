mod test_ar;
mod test_javadoc;
mod test_pyc;

use anyhow::Result;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use tempfile::TempDir;

use add_determinism::handlers;
use add_determinism::options;

fn prepare_dir(path: &str) -> Result<(Box<TempDir>, Box<PathBuf>)> {
    let dir = TempDir::new()?;
    let input_path = dir.path().join(Path::new(path).file_name().unwrap());
    fs::copy(path, &input_path)?;
    Ok((Box::new(dir), Box::new(input_path)))
}

fn make_handler(
    source_date_epoch: i64,
    func: handlers::HandlerBoxed,
) -> Result<Box<dyn handlers::Processor>> {
    let cfg = Rc::new(options::Config::empty(source_date_epoch));
    let mut handler = func(&cfg);
    handler.initialize()?;
    Ok(handler)
}

struct Trivial {}

impl Trivial {
    pub fn boxed() -> Box<dyn handlers::Processor> {
        Box::new(Self {})
    }
}
impl handlers::Processor for Trivial {
    fn name(&self) -> &str {
        "trivial"
    }

    fn filter(&self, _path: &Path) -> Result<bool> {
        Ok(true)
    }

    fn process(&self, _input_path: &Path) -> Result<bool> {
        Ok(true)
    }
}

#[test]
fn test_inode_map() {
    let (dir, _input) = prepare_dir("tests/cases/libempty.a").unwrap();

    let mut handlers = vec![Trivial::boxed()];
    let mut cache = handlers::inodes_seen();

    let mods = handlers::process_file_or_dir(&handlers, &mut cache, dir.path(), None).unwrap();
    assert_eq!(mods, 1);

    let mods = handlers::process_file_or_dir(&handlers, &mut cache, dir.path(), None).unwrap();
    assert_eq!(mods, 0);

    assert_eq!(cache.len(), 1);

    handlers.push(Trivial::boxed());

    let mods = handlers::process_file_or_dir(&handlers, &mut cache, dir.path(), None).unwrap();
    assert_eq!(mods, 1);

    let mods = handlers::process_file_or_dir(&handlers, &mut cache, dir.path(), None).unwrap();
    assert_eq!(mods, 0);

    assert_eq!(cache.len(), 1);
}

#[test]
fn test_inode_map_2() {
    let (dir, _input) = prepare_dir("tests/cases/testrelro.a").unwrap();

    let cfg = Rc::new(options::Config::empty(111));
    let ar = handlers::ar::Ar::boxed(&cfg);

    let handlers = vec![ar];
    let mut cache = handlers::inodes_seen();

    let mods = handlers::process_file_or_dir(&handlers, &mut cache, dir.path(), None).unwrap();
    assert_eq!(mods, 1);

    let mods = handlers::process_file_or_dir(&handlers, &mut cache, dir.path(), None).unwrap();
    assert_eq!(mods, 0); // The file was already processed, so no change

    // The inode changes because we rewrite the file
    assert_eq!(cache.len(), 2);
}
