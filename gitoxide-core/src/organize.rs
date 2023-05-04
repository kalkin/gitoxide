use moonwalk::{DirEntry, WalkState};
use std::ffi::OsString;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

use gix::{objs::bstr::ByteSlice, progress, Progress};

#[derive(Default, Copy, Clone, Eq, PartialEq)]
pub enum Mode {
    Execute,
    #[default]
    Simulate,
}

fn find_git_repository_workdirs<P: Progress>(
    root: impl AsRef<Path>,
    mut progress: P,
    debug: bool,
    threads: Option<usize>,
) -> anyhow::Result<Vec<(PathBuf, gix::Kind)>>
where
    P::SubProgress: Sync,
{
    progress.init(None, progress::count("filesystem items"));
    fn is_repository(path: &Path, is_dir: bool) -> Option<gix::Kind> {
        if is_dir {
            if path.file_name() == Some(OsStr::new(".git")) {
                gix::discover::is_git(&path).ok().map(Into::into)
            } else {
                let git_dir = path.join(".git");
                let meta = git_dir.metadata().ok()?;
                if meta.is_file() {
                    Some(gix::Kind::WorkTree { is_linked: true })
                } else if meta.is_dir() {
                    if git_dir.join("HEAD").is_file() && git_dir.join("config").is_file() {
                        gix::discover::is_git(&git_dir).ok().map(Into::into)
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
        } else if path.file_name() == Some(OsStr::new(".git")) {
            Some(gix::Kind::WorkTree { is_linked: true })
        } else {
            None
        }
    }
    fn into_workdir(git_dir: PathBuf) -> PathBuf {
        if gix::discover::is_bare(&git_dir) {
            git_dir
        } else {
            git_dir.parent().expect("git is never in the root").to_owned()
        }
    }

    let entries = std::sync::Mutex::new(Vec::new());
    let seen = AtomicUsize::default();
    #[derive(Clone)]
    struct Delegate<'a> {
        path: PathBuf,
        entries: &'a std::sync::Mutex<Vec<(PathBuf, gix::Kind)>>,
        seen: &'a AtomicUsize,
        debug: bool,
    }

    let mut walk = moonwalk::WalkBuilder::new();
    walk.follow_links(false);
    let root = root.as_ref();
    walk.run_parallel(
        root.as_ref(),
        threads.unwrap_or(0),
        Delegate {
            path: Default::default(),
            entries: &entries,
            seen: &seen,
            debug,
        },
        root.as_os_str().to_owned(),
    )?;

    if debug {
        dbg!(seen.load(Ordering::Relaxed));
    }
    return Ok(entries.into_inner()?);

    impl<'b> moonwalk::VisitorParallel for Delegate<'b> {
        type State = OsString;

        fn visit<'a>(
            &mut self,
            parents: impl Iterator<Item = &'a Self::State>,
            dent: std::io::Result<&mut DirEntry<'_>>,
        ) -> WalkState<Self::State> {
            match dent {
                Ok(dent) => {
                    self.seen.fetch_add(1, Ordering::SeqCst);
                    self.path.clear();
                    self.path.extend(parents.collect::<Vec<_>>().into_iter().rev());
                    self.path.push(dent.file_name());
                    if self.debug && dent.file_type().is_dir() {
                        eprintln!("{}", self.path.display());
                    }
                    if let Some(kind) = is_repository(&self.path, dent.file_type().is_dir()) {
                        self.entries
                            .lock()
                            .unwrap()
                            .push((into_workdir(self.path.clone()), kind));
                        WalkState::Skip
                    } else {
                        WalkState::Continue(dent.file_name().to_owned())
                    }
                }
                Err(_err) => WalkState::Skip,
            }
        }

        fn pop_dir<'a>(&mut self, _state: Self::State, _parents: impl Iterator<Item = &'a Self::State>) {}
    }
}

fn find_origin_remote(repo: &Path) -> anyhow::Result<Option<gix_url::Url>> {
    let non_bare = repo.join(".git").join("config");
    let local = gix::config::Source::Local;
    let config = gix::config::File::from_path_no_includes(non_bare.as_path(), local)
        .or_else(|_| gix::config::File::from_path_no_includes(repo.join("config").as_path(), local))?;
    Ok(config
        .string_by_key("remote.origin.url")
        .map(|url| gix_url::Url::from_bytes(url.as_ref()))
        .transpose()?)
}

fn handle(
    mode: Mode,
    kind: gix::Kind,
    git_workdir: &Path,
    canonicalized_destination: &Path,
    progress: &mut impl Progress,
) -> anyhow::Result<()> {
    if let gix::Kind::WorkTree { is_linked: true } = kind {
        return Ok(());
    }
    fn to_relative(path: PathBuf) -> PathBuf {
        path.components()
            .skip_while(|c| c == &std::path::Component::RootDir)
            .collect()
    }

    fn find_parent_repo(mut git_workdir: &Path) -> Option<PathBuf> {
        while let Some(parent) = git_workdir.parent() {
            let has_contained_git_folder_or_file = std::fs::read_dir(parent).ok()?.any(|e| {
                e.ok()
                    .and_then(|e| {
                        e.file_name()
                            .to_str()
                            .map(|name| name == ".git" && e.path() != git_workdir)
                    })
                    .unwrap_or(false)
            });
            if has_contained_git_folder_or_file {
                return Some(parent.to_owned());
            }
            git_workdir = parent;
        }
        None
    }

    if let Some(parent_repo_path) = find_parent_repo(git_workdir) {
        progress.fail(format!(
            "Skipping repository at {:?} as it is nested within repository {:?}",
            git_workdir.display(),
            parent_repo_path
        ));
        return Ok(());
    }

    let url = match find_origin_remote(git_workdir)? {
        None => {
            progress.info(format!(
                "Skipping repository {:?} without 'origin' remote",
                git_workdir.display()
            ));
            return Ok(());
        }
        Some(url) => url,
    };
    if url.path.is_empty() {
        progress.info(format!(
            "Skipping repository at {:?} whose remote does not have a path: {:?}",
            git_workdir.display(),
            url.to_bstring()
        ));
        return Ok(());
    }

    let destination = canonicalized_destination
        .join(match url.host() {
            Some(h) => h,
            None => return Ok(()),
        })
        .join(to_relative({
            let mut path = gix_url::expand_path(None, url.path.as_bstr())?;
            match kind {
                gix::Kind::Submodule => {
                    unreachable!("BUG: We should not try to relocated submodules and not find them the first place")
                }
                gix::Kind::Bare => path,
                gix::Kind::WorkTree { .. } => {
                    if let Some(ext) = path.extension() {
                        if ext == "git" {
                            path.set_extension("");
                        }
                    }
                    path
                }
            }
        }));

    if let Ok(destination) = destination.canonicalize() {
        if git_workdir.canonicalize()? == destination {
            return Ok(());
        }
    }
    match mode {
        Mode::Simulate => progress.info(format!(
            "WOULD move {} to {}",
            git_workdir.display(),
            destination.display()
        )),
        Mode::Execute => {
            std::fs::create_dir_all(destination.parent().expect("repo destination is not the root"))?;
            progress.done(format!("Moving {} to {}", git_workdir.display(), destination.display()));
            std::fs::rename(git_workdir, &destination)?;
        }
    }
    Ok(())
}

/// Find all working directories in the given `source_dir` and print them to `out` while providing `progress`.
pub fn discover<P: Progress>(
    source_dir: impl AsRef<Path>,
    mut out: impl std::io::Write,
    mut progress: P,
    debug: bool,
    threads: Option<usize>,
) -> anyhow::Result<()>
where
    <P::SubProgress as Progress>::SubProgress: Sync,
{
    for (git_workdir, _kind) in
        find_git_repository_workdirs(source_dir, progress.add_child("Searching repositories"), debug, threads)?
    {
        writeln!(&mut out, "{}", git_workdir.display())?;
    }
    Ok(())
}

pub fn run<P: Progress>(
    mode: Mode,
    source_dir: impl AsRef<Path>,
    destination: impl AsRef<Path>,
    mut progress: P,
    threads: Option<usize>,
) -> anyhow::Result<()>
where
    <P::SubProgress as Progress>::SubProgress: Sync,
{
    let mut num_errors = 0usize;
    let destination = destination.as_ref().canonicalize()?;
    for (path_to_move, kind) in
        find_git_repository_workdirs(source_dir, progress.add_child("Searching repositories"), false, threads)?
    {
        if let Err(err) = handle(mode, kind, &path_to_move, &destination, &mut progress) {
            progress.fail(format!(
                "Error when handling directory {:?}: {}",
                path_to_move.display(),
                err
            ));
            num_errors += 1;
        }
    }

    if num_errors > 0 {
        anyhow::bail!("Failed to handle {} repositories", num_errors)
    } else {
        Ok(())
    }
}
