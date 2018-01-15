extern crate bincode;
extern crate cargo;
extern crate daggy;
extern crate failure;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate walkdir;

use bincode::{serialize_into, Infinite};
use cargo::core::{Dependency, PackageId, Source, SourceId, Summary};
use cargo::util::config::Config;
use daggy::Dag;
use failure::{Error, SyncFailure};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use walkdir::WalkDir;

trait ResultExt<T, E> {
    fn sync(self) -> Result<T, SyncFailure<E>>
    where
        Self: Sized,
        E: ::std::error::Error + Send + 'static;
}

impl<T, E> ResultExt<T, E> for Result<T, E> {
    fn sync(self) -> Result<T, SyncFailure<E>>
    where
        Self: Sized,
        E: ::std::error::Error + Send + 'static,
    {
        self.map_err(SyncFailure::new)
    }
}

fn list_registry_crates<P: AsRef<Path>>(regpath: P) -> Vec<String> {
    WalkDir::new(&regpath)
        .into_iter()
        .filter_map(|e| e.ok()
                    .and_then(|f| {
                        if f.file_type().is_file() && f.depth() > 1 {
                            Some(f.file_name().to_owned().to_string_lossy().into_owned())
                        } else {
                            None
                        }
                    }))
        .collect()
}
/*
fn filter_dependencies<'s, F>(mut source: &'s mut Source, crates: Vec<String>, filter: F) -> Box<Iterator<Item=Dependency> + 's>
    where F: Fn(&Dependency) -> bool,
          F: 's,
{
    Box::new(crates.into_iter()
             .filter_map(move |name| {
                 let krate = Dependency::parse_no_deprecated(&name,
                                                             None,
                                                             source.source_id()).unwrap();
                 source.query_vec(&krate).ok().map(|versions| (krate, versions))
             })
             .filter(move |&(_, ref versions)| {
                 versions.iter().any(|v: &Summary| v.dependencies().iter().any(&filter))
             })
             .map(|(krate, _)| krate)
    )
}
 */


#[derive(PartialEq, Serialize)]
struct Crate {
    #[serde(with = "SummaryDef")]
    summary: Summary,
}

impl Crate {
    fn new(summary: Summary) -> Crate {
        Crate { summary }
    }
}

fn summary_dependencies(summary: &Summary) -> Vec<DependencyDef> {
    summary.dependencies().iter().map(|d| d.clone().into()).collect()
}

#[derive(Serialize)]
#[serde(remote = "Summary")]
struct SummaryDef {
    #[serde(getter = "Summary::package_id")]
    pkg_id: PackageId,
    #[serde(getter = "summary_dependencies")]
    dependencies: Vec<DependencyDef>,
    #[serde(getter = "Summary::features")]
    features: BTreeMap<String, Vec<String>>,
}

impl From<SummaryDef> for Summary {
    fn from(s: SummaryDef) -> Summary {
        let SummaryDef { pkg_id, dependencies, features } = s;
        let dependencies = dependencies.into_iter().map(|d| d.into()).collect();
        Summary::new(pkg_id, dependencies, features).unwrap()
    }
}

#[derive(Serialize)]
struct DependencyDef {
    dependency: Dependency,
}

impl From<Dependency> for DependencyDef {
    fn from(dependency: Dependency) -> DependencyDef {
        DependencyDef { dependency }
    }
}

impl Into<Dependency> for DependencyDef {
    fn into(self) -> Dependency {
        self.dependency
    }
}

impl Eq for Crate {}

impl Ord for Crate {
    fn cmp(&self, other: &Crate) -> Ordering {
        self.summary.package_id().cmp(&other.summary.package_id())
    }
}

impl PartialOrd for Crate {
    fn partial_cmp(&self, other: &Crate) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl From<Summary> for Crate {
    fn from(s: Summary) -> Crate {
        Crate::new(s)
    }
}

/*
impl Hash for Crate {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
        self.phone.hash(state);
    }
}
 */

//type CrateKey = (String, Version);
//type CrateMap = HashMap<CrateKey, Summary>;
type CrateSet = BTreeSet<Crate>;

/// Format `duration` as seconds with a fractional component.
fn fmt_duration_as_secs(duration: &Duration) -> String
{
    format!("{}.{:03} s", duration.as_secs(), duration.subsec_nanos() / 1000_000)
}

fn find_crates<P>(regpath: P, source: &mut Source) -> Result<CrateSet, Error>
    where P: AsRef<Path>,
{
    // List crates by recursively walking the index.
    let start = Instant::now();
    let crate_names = list_registry_crates(regpath);
    println!("Found {} crates in {}", crate_names.len(), fmt_duration_as_secs(&start.elapsed()));
    // First, ask the registry to load all crates, and build a HashMap of
    // (name, version) -> Summary
    let start = Instant::now();
    let crates: CrateSet = crate_names.into_iter().filter_map(move |name| {
        let krate = Dependency::parse_no_deprecated(&name,
                                                    None,
                                                    source.source_id()).unwrap();
        source.query_vec(&krate).ok()
    }).flat_map(|summaries| summaries.into_iter().map(|s| s.into())).collect();
    println!("Loaded {} crate versions in {}", crates.len(),
             fmt_duration_as_secs(&start.elapsed()));
    Ok(crates)
}

fn load_cached_crate_list<P>(path: P) -> Result<CrateSet, Error>
    where P: AsRef<Path>,
{
    unimplemented!()
}

fn save_crate_list<P>(crates: &CrateSet, path: P) -> Result<(), Error>
    where P: AsRef<Path>,
{
    let start = Instant::now();
    let mut f = File::create(path)?;
    serialize_into(&mut f, crates, Infinite)?;
    println!("Saved index cache in {}", fmt_duration_as_secs(&start.elapsed()));
    Ok(())
}

fn get_crate_list(config: &Config, source: &mut Source) -> Result<CrateSet, Error> {
    let regpath = config.registry_index_path().into_path_unlocked();
    let mut cache_filename = OsString::from(regpath.file_name().unwrap());
    cache_filename.push(".cache");
    let cache_filename = PathBuf::from(cache_filename);
    if cache_filename.exists() {
        load_cached_crate_list(&cache_filename)
    } else {
        let crates = find_crates(regpath, source)?;
        save_crate_list(&crates, &cache_filename)?;
        Ok(crates)
    }
}

fn work() -> Result<(), Error> {
    let config = Config::default().sync()?;
    let sid = SourceId::crates_io(&config).sync()?;
    let mut source = sid.load(&config).sync()?;
    let crates = get_crate_list(&config, &mut source)?;

    /*
    let res: Vec<_> = filter_dependencies(&mut source,
                                          crates,
                                          |dep| dep.name() == "cc" || dep.name() == "gcc")
        .collect();
*/
    Ok(())
}

fn main() {
    work().unwrap();
}
