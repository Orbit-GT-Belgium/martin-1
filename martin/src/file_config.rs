use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::mem;
use std::path::PathBuf;

use futures::TryFutureExt;
use log::{info, warn};
use serde::{Deserialize, Serialize};

use crate::config::{copy_unrecognized_config, UnrecognizedValues};
use crate::file_config::FileError::{InvalidFilePath, InvalidSourceFilePath, IoError};
use crate::source::{Source, Sources, Xyz};
use crate::utils::{sorted_opt_map, Error, IdResolver, OneOrMany};
use crate::OneOrMany::{Many, One};

#[derive(thiserror::Error, Debug)]
pub enum FileError {
    #[error("IO error {0}: {}", .1.display())]
    IoError(std::io::Error, PathBuf),

    #[error("Source path is not a file: {}", .0.display())]
    InvalidFilePath(PathBuf),

    #[error("Source {0} uses bad file {}", .1.display())]
    InvalidSourceFilePath(String, PathBuf),

    #[error(r"Unable to parse metadata in file {}: {0}", .1.display())]
    InvalidMetadata(String, PathBuf),

    #[error(r#"Tile {0:#} not found in {1}"#)]
    GetTileError(Xyz, String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FileConfigEnum {
    Path(PathBuf),
    Paths(Vec<PathBuf>),
    Config(FileConfig),
}

impl FileConfigEnum {
    #[must_use]
    pub fn new(paths: Vec<PathBuf>) -> Option<FileConfigEnum> {
        Self::new_extended(paths, HashMap::new(), UnrecognizedValues::new())
    }

    #[must_use]
    pub fn new_extended(
        paths: Vec<PathBuf>,
        configs: HashMap<String, FileConfigSrc>,
        unrecognized: UnrecognizedValues,
    ) -> Option<FileConfigEnum> {
        if configs.is_empty() && unrecognized.is_empty() {
            match paths.len() {
                0 => None,
                1 => Some(FileConfigEnum::Path(paths.into_iter().next().unwrap())),
                _ => Some(FileConfigEnum::Paths(paths)),
            }
        } else {
            Some(FileConfigEnum::Config(FileConfig {
                paths: OneOrMany::new_opt(paths),
                sources: if configs.is_empty() {
                    None
                } else {
                    Some(configs)
                },
                unrecognized,
            }))
        }
    }

    pub fn extract_file_config(&mut self) -> FileConfig {
        match self {
            FileConfigEnum::Path(path) => FileConfig {
                paths: Some(One(mem::take(path))),
                ..FileConfig::default()
            },
            FileConfigEnum::Paths(paths) => FileConfig {
                paths: Some(Many(mem::take(paths))),
                ..Default::default()
            },
            FileConfigEnum::Config(cfg) => mem::take(cfg),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FileConfig {
    /// A list of file paths
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paths: Option<OneOrMany<PathBuf>>,
    /// A map of source IDs to file paths or config objects
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(serialize_with = "sorted_opt_map")]
    pub sources: Option<HashMap<String, FileConfigSrc>>,
    #[serde(flatten)]
    pub unrecognized: UnrecognizedValues,
}

impl FileConfig {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.paths.is_none() && self.sources.is_none()
    }
}

/// A serde helper to store a boolean as an object.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FileConfigSrc {
    Path(PathBuf),
    Obj(FileConfigSource),
}

impl FileConfigSrc {
    pub fn abs_path(&self) -> Result<PathBuf, FileError> {
        let path = match self {
            Self::Path(p) => p,
            Self::Obj(o) => &o.path,
        };
        path.canonicalize().map_err(|e| IoError(e, path.clone()))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FileConfigSource {
    pub path: PathBuf,
}

impl FileConfigEnum {
    pub fn finalize(&self, prefix: &str) -> Result<UnrecognizedValues, Error> {
        let mut res = UnrecognizedValues::new();
        if let Self::Config(cfg) = self {
            copy_unrecognized_config(&mut res, prefix, &cfg.unrecognized);
        }
        Ok(res)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Path(_) => false,
            Self::Paths(v) => v.is_empty(),
            Self::Config(c) => c.is_empty(),
        }
    }
}

pub async fn resolve_files<Fut>(
    config: &mut Option<FileConfigEnum>,
    idr: IdResolver,
    extension: &str,
    create_source: &mut impl FnMut(String, PathBuf) -> Fut,
) -> Result<Sources, Error>
where
    Fut: Future<Output = Result<Box<dyn Source>, FileError>>,
{
    resolve_int(config, idr, extension, create_source)
        .map_err(crate::Error::from)
        .await
}

async fn resolve_int<Fut>(
    config: &mut Option<FileConfigEnum>,
    idr: IdResolver,
    extension: &str,
    create_source: &mut impl FnMut(String, PathBuf) -> Fut,
) -> Result<Sources, FileError>
where
    Fut: Future<Output = Result<Box<dyn Source>, FileError>>,
{
    let Some(cfg) = config else { return Ok(Sources::default()) };
    let cfg = cfg.extract_file_config();

    let mut results = Sources::default();
    let mut configs = HashMap::new();
    let mut files = HashSet::new();
    let mut directories = Vec::new();

    if let Some(sources) = cfg.sources {
        for (id, source) in sources {
            let can = source.abs_path()?;
            if !can.is_file() {
                // todo: maybe warn instead?
                return Err(InvalidSourceFilePath(id.to_string(), can));
            }

            let dup = !files.insert(can.clone());
            let dup = if dup { "duplicate " } else { "" };
            let id = idr.resolve(&id, can.to_string_lossy().to_string());
            info!("Configured {dup}source {id} from {}", can.display());
            configs.insert(id.clone(), source.clone());

            let path = match source {
                FileConfigSrc::Obj(pmt) => pmt.path,
                FileConfigSrc::Path(path) => path,
            };
            results.insert(id.clone(), create_source(id, path).await?);
        }
    }

    if let Some(paths) = cfg.paths {
        for path in paths {
            let is_dir = path.is_dir();
            let dir_files = if is_dir {
                // directories will be kept in the config just in case there are new files
                directories.push(path.clone());
                path.read_dir()
                    .map_err(|e| IoError(e, path.clone()))?
                    .filter_map(Result::ok)
                    .filter(|f| {
                        f.path().extension().filter(|e| *e == extension).is_some()
                            && f.path().is_file()
                    })
                    .map(|f| f.path())
                    .collect()
            } else if path.is_file() {
                vec![path]
            } else {
                return Err(InvalidFilePath(path.canonicalize().unwrap_or(path)));
            };
            for path in dir_files {
                let can = path.canonicalize().map_err(|e| IoError(e, path.clone()))?;
                if files.contains(&can) {
                    if !is_dir {
                        warn!("Ignoring duplicate MBTiles path: {}", can.display());
                    }
                    continue;
                }
                let id = path.file_stem().map_or_else(
                    || "_unknown".to_string(),
                    |s| s.to_string_lossy().to_string(),
                );
                let source = FileConfigSrc::Path(path);
                let id = idr.resolve(&id, can.to_string_lossy().to_string());
                info!("Configured source {id} from {}", can.display());
                files.insert(can);
                configs.insert(id.clone(), source.clone());

                let path = match source {
                    FileConfigSrc::Obj(pmt) => pmt.path,
                    FileConfigSrc::Path(path) => path,
                };
                results.insert(id.clone(), create_source(id, path).await?);
            }
        }
    }

    *config = FileConfigEnum::new_extended(directories, configs, cfg.unrecognized);

    Ok(results)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use indoc::indoc;

    use crate::file_config::{FileConfigEnum, FileConfigSource, FileConfigSrc};

    #[test]
    fn parse() {
        let cfg = serde_yaml::from_str::<FileConfigEnum>(indoc! {"
            paths:
              - /dir-path
              - /path/to/file2.ext
            sources:
                pm-src1: /tmp/file.ext
                pm-src2:
                  path: /tmp/file.ext
        "})
        .unwrap();
        let res = cfg.finalize("").unwrap();
        assert!(res.is_empty(), "unrecognized config: {res:?}");
        let FileConfigEnum::Config(cfg) = cfg else {
            panic!();
        };
        let paths = cfg.paths.clone().unwrap().into_iter().collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/dir-path"),
                PathBuf::from("/path/to/file2.ext")
            ]
        );
        assert_eq!(
            cfg.sources,
            Some(HashMap::from_iter(vec![
                (
                    "pm-src1".to_string(),
                    FileConfigSrc::Path(PathBuf::from("/tmp/file.ext"))
                ),
                (
                    "pm-src2".to_string(),
                    FileConfigSrc::Obj(FileConfigSource {
                        path: PathBuf::from("/tmp/file.ext"),
                    })
                )
            ]))
        );
    }
}
