/* SPDX-License-Identifier: GPL-3.0-or-later */

pub mod ar;
pub mod jar;
pub mod javadoc;
pub mod pyc;

use anyhow::{anyhow, Context, Result};
use log::{debug, info, warn};
use std::collections::HashMap;
use std::fs;
use std::fs::{File, Metadata};
use std::io;
use std::io::Seek;
use std::os::linux::fs::MetadataExt as _;
use std::os::unix::fs as unix_fs;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::options;

pub trait Processor {
    fn name(&self) -> &str;

    /// Optionally, do "global" setup of the processor.
    fn initialize(&mut self) -> Result<()> {
        Ok(())
    }

    /// Return true if the given path looks like it should be processed.
    fn filter(&self, path: &Path) -> Result<bool>;

    /// Process file and return true if modifications were made.
    fn process(&self, path: &Path) -> Result<bool>;
}

// pub state: Option<Box<dyn ProcessorState>>,

// pub trait ProcessorState {}

pub type HandlerBoxed = fn(&Rc<options::Config>) -> Box<dyn Processor>;

pub const HANDLERS: &[(&str, HandlerBoxed)] = &[
    ("ar", ar::Ar::boxed),
    ("jar", jar::Jar::boxed),
    ("javadoc", javadoc::Javadoc::boxed),
    ("pyc", pyc::Pyc::boxed),
];

pub fn handler_names() -> Vec<&'static str> {
    HANDLERS.iter().map(|(name, _)| *name).collect()
}

pub fn make_handlers(config: &Rc<options::Config>) -> Result<Vec<Box<dyn Processor>>> {
    let mut handlers: Vec<Box<dyn Processor>> = vec![];

    for (name, func) in HANDLERS {
        if config.handler_names.contains(name) {
            let mut handler = func(config);
            match handler.initialize() {
                Err(e) => {
                    if config.strict_handlers {
                        let e = anyhow!("Cannot initialize handler {}: {}", handler.name(), e);
                        return Err(e);
                    }
                    warn!("Handler {} skipped: {}", handler.name(), e);
                }
                Ok(()) => {
                    debug!("Initialized handler {}.", handler.name());
                    handlers.push(handler);
                }
            }
        }
    }

    Ok(handlers)
}

pub fn inodes_seen() -> HashMap<u64, u8> {
    HashMap::new()
}

pub fn do_normal_work(config: options::Config) -> Result<u64> {
    let config = Rc::new(config);

    let handlers = make_handlers(&config)?;
    let mut inodes_seen = inodes_seen();
    let mut n_paths = 0;

    for input_path in &config.inputs {
        match process_file_or_dir(&handlers, &mut inodes_seen, input_path, None) {
            Err(err) => {
                warn!("{}: failed to process: {}", input_path.display(), err);
            }
            Ok(num) => {
                n_paths += num;
            }
        }
    }

    Ok(n_paths)
}

pub type ProcessWrapper<'a> = Option<&'a dyn Fn(u8, &Path) -> Result<()>>;

fn process_file(
    handlers: &[Box<dyn Processor>],
    already_seen: &mut u8,
    input_path: &Path,
    process_wrapper: ProcessWrapper,
) -> Result<bool> {
    // When processing locally, this says whether modifications have
    // been made. When processing remotely, it just says whether we
    // requested some processing.
    let mut entry_mod = false;

    let mut selected_handlers = 0;

    for (n_processor, processor) in handlers.iter().enumerate() {
        // The same inode can be linked under multiple names
        // with different extensions. Thus, we check if the
        // given processor already handled this file.
        if *already_seen & (1 << n_processor) > 0 {
            debug!(
                "{}: already seen by {} handler",
                input_path.display(),
                processor.name()
            );
            continue;
        }

        let cond = processor.filter(input_path)?;
        debug!(
            "{}: handler {}: {}",
            input_path.display(),
            processor.name(),
            cond
        );

        if cond {
            selected_handlers |= 1 << n_processor;

            if process_wrapper.is_none() {
                match processor.process(input_path) {
                    Err(err) => {
                        warn!("{}: failed to process: {}", input_path.display(), err);
                    }
                    Ok(false) => {}
                    Ok(true) => {
                        entry_mod = true;
                    }
                }
            }
        }

        *already_seen |= selected_handlers;
    }

    if let Some(wrapper) = process_wrapper {
        if selected_handlers > 0 {
            wrapper(selected_handlers, input_path)?;
            entry_mod = true;
        }
    }

    Ok(entry_mod)
}

pub fn process_file_or_dir(
    handlers: &[Box<dyn Processor>],
    inodes_seen: &mut HashMap<u64, u8>,
    input_path: &Path,
    process_wrapper: ProcessWrapper,
) -> Result<u64> {
    let mut first = true; // WalkDir doesn't allow handling the original argument
                          // differently from any subdirectories, but we want to return
                          // an error if the specified path is missing or inaccessible,
                          // so keep a flag to tell us if we're looking at the first
                          // entry.
    let mut modifications = 0;

    for entry in walkdir::WalkDir::new(input_path)
        .follow_links(false)
        .into_iter()
    {
        let entry = match entry {
            Err(e) => {
                if first {
                    return Err(anyhow!("Cannot open path: {}", e));
                } else {
                    warn!("Cannot open path: {}", e);
                    continue;
                }
            }
            Ok(entry) => entry,
        };

        first = false;

        debug!("Looking at {}…", entry.path().display());

        let metadata = entry.metadata()?;
        if !metadata.is_file() {
            debug!("{}: not a file", entry.path().display());
            continue;
        }

        let inode = metadata.ino();
        let mut already_seen = *inodes_seen.get(&inode).unwrap_or(&0);

        let entry_mod = process_file(handlers, &mut already_seen, entry.path(), process_wrapper)?;

        inodes_seen.insert(inode, already_seen); // This is the orig inode
        if entry_mod {
            modifications += 1;

            // The path might have been replaced with a new inode.
            let metadata = entry.metadata()?;
            let inode2 = metadata.ino();
            if inode2 != inode {
                // This is the new inode. We use the same set of bits in
                // already_seen, because those handlers have already been
                // applied to the contents of the new inode.
                inodes_seen.insert(inode2, already_seen);
            }
        }
    }

    Ok(modifications)
}

pub struct InputOutputHelper<'a> {
    input_path: &'a Path,
    input_metadata: Metadata,

    output_path: Option<PathBuf>,
    output: Option<File>,
}

impl<'a> InputOutputHelper<'a> {
    pub fn open(input_path: &'a Path) -> Result<(Self, File)> {
        let input =
            File::open(input_path).with_context(|| format!("Cannot open {:?}", input_path))?;
        // I tried using BufReader, but it returns short reads occasionally.

        let input_metadata = input.metadata()?;

        let io = InputOutputHelper {
            input_path,
            input_metadata,
            output_path: None,
            output: None,
        };

        Ok((io, input))
    }

    pub fn open_output(&mut self) -> Result<()> {
        assert!(self.output_path.is_none());
        assert!(self.output.is_none());

        let input_file_name = match self.input_path.file_name().unwrap().to_str() {
            Some(name) => name,
            None => {
                return Err(anyhow!("Invalid file name {:?}", self.input_path));
            }
        };

        let output_path = self
            .input_path
            .with_file_name(format!(".#.{}.tmp", input_file_name));

        let mut openopts = File::options();
        openopts.read(true).write(true).create_new(true);

        let output = match openopts.open(&output_path) {
            Ok(some) => some,
            Err(e) => {
                if e.kind() != io::ErrorKind::AlreadyExists {
                    return Err(anyhow!(
                        "{}: cannot open temporary file: {}",
                        output_path.display(),
                        e
                    ));
                }

                debug!(
                    "{}: stale temporary file found, removing",
                    output_path.display()
                );
                fs::remove_file(&output_path)?;
                openopts.open(&output_path)?
            }
        };
        self.output_path = Some(output_path);
        self.output = Some(output);

        Ok(())
    }

    pub fn finalize(&mut self, have_mod: bool) -> Result<bool> {
        let meta = &self.input_metadata;

        if have_mod {
            let output_path = self.output_path.as_ref().unwrap();

            let mut output = self.output.as_mut();
            let mut fallback_output;

            if output.is_none() {
                fallback_output = match File::open(output_path) {
                    Ok(some) => Some(some),
                    Err(e) => {
                        if e.kind() == io::ErrorKind::NotFound {
                            return Ok(false); // no modifications and nothing to do
                        } else {
                            return Err(anyhow!(
                                "{}: cannot reopen temporary file: {}",
                                output_path.display(),
                                e
                            ));
                        }
                    }
                };
                output = fallback_output.as_mut();
            }
            let output = output.unwrap();

            // If the original file has nlinks == 1, we atomically replace it.
            // If it has multiple links, we reopen the orignal file and rewrite it.
            // This way the inode number is retained and hard links are not broken.
            if meta.nlink() == 1 {
                output.set_permissions(meta.permissions())?;
                output.set_modified(meta.modified()?)?;

                if let Err(e) =
                    unix_fs::lchown(output_path, Some(meta.st_uid()), Some(meta.st_gid()))
                {
                    if e.kind() == io::ErrorKind::PermissionDenied {
                        warn!(
                            "{}: cannot change file ownership, ignoring",
                            self.input_path.display()
                        );
                    } else {
                        return Err(anyhow!(
                            "{}: cannot change file ownership: {}",
                            self.input_path.display(),
                            e
                        ));
                    }
                }

                info!(
                    "{}: replacing with normalized version",
                    self.input_path.display()
                );
                fs::rename(output_path, self.input_path)?;
            } else {
                output.seek(io::SeekFrom::Start(0))?;

                let mut input_writer = File::options().write(true).open(self.input_path)?;

                info!(
                    "{}: rewriting with normalized contents",
                    self.input_path.display()
                );
                io::copy(output, &mut input_writer)?;

                debug!("{}: unlinking", output_path.display());
                if let Err(e) = fs::remove_file(output_path) {
                    if e.kind() != io::ErrorKind::NotFound {
                        warn!("Failed to remove {}: {}", self.input_path.display(), e);
                    }
                }

                input_writer.set_modified(meta.modified()?)?;
            }
        } else if let Some(output_path) = &self.output_path {
            debug!("{}: discarding modified version", self.input_path.display());
            fs::remove_file(output_path)?;
        }

        Ok(have_mod)
    }
}
