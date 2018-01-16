extern crate cargo;
extern crate daggy;
#[macro_use]
extern crate failure;
extern crate fallible_iterator;
extern crate semver;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;
extern crate walkdir;

use cargo::core::SourceId;
use cargo::util::config::Config;
use cargo::util::hex;
use daggy::Dag;
use failure::{Error, ResultExt, SyncFailure};
use fallible_iterator::FallibleIterator;
use semver::{Version, VersionReq};
use std::boxed::Box;
use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader};
use std::path::{Path};
use std::time::{Duration, Instant};
use walkdir::WalkDir;

trait ResultExt2<T, E> {
    fn sync(self) -> Result<T, SyncFailure<E>>
    where
        Self: Sized,
        E: ::std::error::Error + Send + 'static;
}

impl<T, E> ResultExt2<T, E> for Result<T, E> {
    fn sync(self) -> Result<T, SyncFailure<E>>
    where
        Self: Sized,
        E: ::std::error::Error + Send + 'static,
    {
        self.map_err(SyncFailure::new)
    }
}

fn read_crate_json<P>(path: P) -> Result<Crate, Error>
    where P: AsRef<Path>,
{
    let path = path.as_ref();
    let f = BufReader::new(File::open(path)?);
    let mut versions: Vec<_> =
        fallible_iterator::convert::<CrateVersion, Error, _>(
            f.lines().map(|l| {
                let l = l?;
                Ok(serde_json::from_reader(l.as_bytes()).with_context(|e| {
                    format!("Error reading line from {:?}: {}, `{}`",
                            path, e, l)
                })?)
            })).collect()?;
    versions.reverse();
    let name = versions.first().unwrap().name.clone();
    Ok(Crate {
        name,
        versions,
    })
}

fn list_registry_crates<P: AsRef<Path>>(regpath: P) -> Box<FallibleIterator<Item=Crate, Error=Error>> {
    Box::new(fallible_iterator::convert(
        WalkDir::new(&regpath)
            .into_iter()
            .filter_entry(|e| e.file_name() != ".git")
            .filter_map(|e| e.ok().and_then(|f| {
                if f.file_type().is_file() && f.depth() > 1 {
                    Some(read_crate_json(f.path()))
                } else {
                    None
                }
            }))))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Dependency {
    name: String,
    req: VersionReq,
    features: Vec<String>,
    optional: bool,
    default_features: bool,
    target: Option<String>,
    kind: Option<String>,
}

impl fmt::Display for Dependency {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} {}", self.name, self.req)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Crate {
    name: String,
    versions: Vec<CrateVersion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CrateVersion {
    name: String,
    #[serde(rename = "vers")]
    version: Version,
    deps: Vec<Dependency>,
    cksum: String,
    features: HashMap<String, Vec<String>>,
    yanked: bool,
}

impl fmt::Display for CrateVersion {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} {}", self.name, self.version)
    }
}

impl Hash for CrateVersion {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
        self.version.hash(state);
    }
}

/// Format `duration` as seconds with a fractional component.
fn fmt_duration_as_secs(duration: &Duration) -> String
{
    format!("{}.{:03} s", duration.as_secs(), duration.subsec_nanos() / 1000_000)
}

fn find_crates<P>(regpath: P) -> Result<Vec<Crate>, Error>
    where P: AsRef<Path>,

{
    // List crates by recursively walking the index.
    let start = Instant::now();
    let crates: Vec<_> = list_registry_crates(regpath).collect()?;
    println!("Found {} crates in {}", crates.len(), fmt_duration_as_secs(&start.elapsed()));
    Ok(crates)
}

// Cribbed from the cargo source.
fn short_name(id: &SourceId) -> String {
    let hash = hex::short_hash(id);
    let ident = id.url().host_str().unwrap_or("").to_string();
    format!("{}-{}", ident, hash)
}

fn work() -> Result<(), Error> {
    let config = Config::default().sync()?;
    let sid = SourceId::crates_io(&config).sync()?;
    let source_name = short_name(&sid);
    let regpath = config.registry_index_path().into_path_unlocked().join(&source_name);
    println!("regpath: {:?}", regpath);
    // Get a vec of all crate versions, and insert them all into the dag.
    let mut dag: Dag<(), ()> = Dag::new();
    let crates: Vec<_> = find_crates(&regpath)?;
    // Lookup by name.
    let mut by_name = HashMap::new();
    let mut crate_nodes = HashMap::new();
    for c in crates.iter() {
        by_name.insert(&c.name, c);
        for v in c.versions.iter() {
            crate_nodes.insert(v, dag.add_node(()));
        }
    }
    let get_dep = |dep: &Dependency| -> Option<&CrateVersion> {
        by_name.get(&dep.name).and_then(|c| {
            c.versions.iter().filter(|v| dep.req.matches(&v.version)).next()
        })
    };
    let start = Instant::now();
    for c in crates.iter() {
        for v in c.versions.iter() {
            let idx = *crate_nodes.get(v).unwrap();
            for dep in v.deps.iter() {
                // Just skip dev deps.
                if let Some("dev") = dep.kind.as_ref().map(String::as_ref) {
                    continue;
                }
                let depver = match get_dep(dep) {
                    Some(s) => s,
                    None => {
                        println!("Failed to find dependency of {}: {}",
                                v, dep);
                        continue;
                    }
                };
                let dep_idx = *crate_nodes.get(depver).unwrap();
                //println!("{} -> {}", v, depver);
                dag.add_edge(idx, dep_idx, ()).with_context(|e| {
                    format!("Failed to add edge from {} to {} ({}): {}",
                            v, dep, depver, e)
                })?;
            }
        }
    }
    println!("Built dag with {} nodes, {} edges in {}",
             dag.node_count(), dag.edge_count(),
             fmt_duration_as_secs(&start.elapsed()));
    Ok(())
}

fn main() {
    match work() {
        Ok(_) => {}
        Err(e) => println!("Error: {}, {}", e.cause(), e.backtrace()),
    }
}
