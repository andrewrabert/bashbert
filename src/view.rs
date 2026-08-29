use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;

use crate::config::{Layer, MountMode, Settings};
use crate::mount::{Entry, PathFilter, Visibility};

pub const TMP: &str = "/tmp";

#[derive(Clone)]
pub enum Filter {
    Unfiltered,
    By(Arc<PathFilter>),
}

impl Filter {
    #[must_use]
    pub fn of_layer(layer: &Layer) -> Self {
        match &layer.filter {
            Some(filter) => Self::By(Arc::clone(filter)),
            None => Self::Unfiltered,
        }
    }

    #[must_use]
    pub fn of_view(settings: &Settings) -> Self {
        match &settings.filter {
            Some(filter) => Self::By(Arc::clone(filter)),
            None => Self::Unfiltered,
        }
    }

    #[must_use]
    pub fn visibility(&self, path: &Path, entry: Entry) -> Visibility {
        match self {
            Self::Unfiltered => Visibility::Shown,
            Self::By(filter) => filter.visibility(path, entry),
        }
    }

    #[must_use]
    pub fn own_visibility(&self, path: &Path, entry: Entry) -> Visibility {
        match self {
            Self::Unfiltered => Visibility::Shown,
            Self::By(filter) => filter.own_visibility(path, entry),
        }
    }
}

pub enum Served {
    Dir { host: PathBuf, mode: MountMode },
    File { host: PathBuf, mode: MountMode },
    Removed(Removal),
}

pub enum Removal {
    Dir,
    File,
    Nothing,
}

pub struct Resolved {
    pub target: PathBuf,
    pub served: Served,
    pub filter: Filter,
}

pub enum Location {
    Host { path: PathBuf, mode: MountMode },
    Unmapped,
}

#[must_use]
pub fn entry_of(host: &Path) -> Entry {
    match std::fs::metadata(host) {
        Ok(meta) if meta.is_dir() => Entry::Dir,
        _ => Entry::File,
    }
}

#[derive(Default)]
pub struct View {
    pub layers: Vec<Resolved>,
}

impl View {
    pub fn push(&mut self, target: PathBuf, served: Served, filter: Filter) {
        self.layers.push(Resolved {
            target,
            served,
            filter,
        });
    }

    #[must_use]
    pub fn locate(&self, tmp: Option<&Path>, vfs: &Path) -> Location {
        if let Some(tmp) = tmp
            && let Ok(rest) = vfs.strip_prefix(TMP)
        {
            return Location::Host {
                path: tmp.join(rest),
                mode: MountMode::ReadWrite,
            };
        }
        for layer in self.layers.iter().rev() {
            match &layer.served {
                Served::Dir { host, mode } => {
                    if let Ok(rest) = vfs.strip_prefix(&layer.target) {
                        let path = host.join(rest);
                        let within = Path::new("/").join(rest);
                        return match layer.filter.visibility(&within, entry_of(&path)) {
                            Visibility::Shown => Location::Host { path, mode: *mode },
                            Visibility::Hidden => Location::Unmapped,
                        };
                    }
                }
                Served::File { host, mode } => {
                    if vfs == layer.target {
                        return Location::Host {
                            path: host.clone(),
                            mode: *mode,
                        };
                    }
                }
                Served::Removed(_) => {
                    if vfs.starts_with(&layer.target) {
                        return Location::Unmapped;
                    }
                }
            }
        }
        Location::Unmapped
    }
}

pub fn scratch_dir() -> anyhow::Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix("bxwrp-")
        .tempdir()
        .context("a sandbox scratch directory")
}

pub async fn read_located(location: Location, vfs: &Path) -> anyhow::Result<String> {
    let Location::Host { path: host, .. } = location else {
        anyhow::bail!("{}: outside every mount", vfs.display());
    };
    let meta = tokio::fs::metadata(&host)
        .await
        .map_err(|_| anyhow::anyhow!("File does not exist."))?;
    anyhow::ensure!(
        !meta.is_dir(),
        "Illegal operation on a directory: {}",
        vfs.display()
    );
    let bytes = tokio::fs::read(&host).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub async fn write_located(
    location: Location,
    vfs: &Path,
    content: &str,
) -> anyhow::Result<crate::host_tool::Written> {
    use crate::host_tool::Written;

    let Location::Host { path: host, mode } = location else {
        anyhow::bail!("{}: outside every mount", vfs.display());
    };
    anyhow::ensure!(
        mode == MountMode::ReadWrite,
        "{}: read-only mount",
        vfs.display()
    );
    let existing = tokio::fs::metadata(&host).await.ok();
    if let Some(meta) = &existing {
        anyhow::ensure!(
            !meta.is_dir(),
            "Illegal operation on a directory: {}",
            vfs.display()
        );
    }
    if let Some(parent) = host.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&host, content).await?;
    Ok(if existing.is_some() {
        Written::Updated
    } else {
        Written::Created
    })
}

pub fn text(path: &Path) -> anyhow::Result<String> {
    path.to_str()
        .map(str::to_string)
        .with_context(|| format!("{}: only UTF-8 paths can be sandboxed", path.display()))
}

pub enum Descend {
    Yes,
    No,
}

pub fn walk(
    host: &Path,
    rel: &Path,
    visit: &mut impl FnMut(&Path, Entry) -> anyhow::Result<Descend>,
) -> anyhow::Result<()> {
    let Ok(entries) = std::fs::read_dir(host.join(rel)) else {
        return Ok(());
    };
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let rel = rel.join(entry.file_name());
        let kind = match entry.file_type() {
            Ok(kind) if kind.is_dir() => Entry::Dir,
            _ => Entry::File,
        };
        if let (Entry::Dir, Descend::Yes) = (kind, visit(&rel, kind)?) {
            walk(host, &rel, visit)?;
        }
    }
    Ok(())
}

pub enum Fate {
    Shown,
    Hidden,
    Absent,
}

pub fn shape(
    fate: impl Fn(&Path, Entry) -> Fate,
    host: &Path,
    target: &Path,
    hide: &mut impl FnMut(Entry, &Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    walk(
        host,
        Path::new(""),
        &mut |rel, entry| match fate(rel, entry) {
            Fate::Shown => Ok(Descend::Yes),
            Fate::Hidden => {
                hide(entry, &target.join(rel))?;
                Ok(Descend::No)
            }
            Fate::Absent => Ok(Descend::No),
        },
    )
}
