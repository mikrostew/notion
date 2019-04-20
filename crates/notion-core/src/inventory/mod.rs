//! Provides types for working with Notion's _inventory_, the local repository
//! of available tool versions.

use std::collections::{BTreeSet, HashSet};
use std::fs::File;
use std::io::Write;
use std::marker::PhantomData;
use std::str::FromStr;
use std::string::ToString;
use std::time::{Duration, SystemTime};

use headers_011::Headers011;
use lazycell::LazyCell;
use reqwest;
use reqwest::hyper_011::header::{CacheControl, CacheDirective, Expires, HttpDate};
use serde_json;
use tempfile::NamedTempFile;

use crate::distro::node::{NodeDistro, NodeVersion};
use crate::distro::package::{PackageDistro, PackageEntry, PackageIndex, PackageVersion};
use crate::distro::yarn::YarnDistro;
use crate::distro::{Distro, Fetched};
use crate::error::ErrorDetails;
use crate::fs::{ensure_containing_dir_exists, read_file_opt};
use crate::hook::ToolHooks;
use crate::path;
use crate::style::progress_spinner;
use crate::version::VersionSpec;
use notion_fail::{throw, Fallible, ResultExt};
use semver::{Version, VersionReq};

pub(crate) mod serial;

#[cfg(feature = "mock-network")]
use mockito;

// ISSUE (#86): Move public repository URLs to config file
cfg_if::cfg_if! {
    if #[cfg(feature = "mock-network")] {
        fn public_node_version_index() -> String {
            format!("{}/node-dist/index.json", mockito::SERVER_URL)
        }
        fn public_yarn_version_index() -> String {
            format!("{}/yarn-releases/index.json", mockito::SERVER_URL)
        }
        fn public_yarn_latest_version() -> String {
            format!("{}/yarn-latest", mockito::SERVER_URL)
        }
        fn public_package_registry_root() -> String {
            format!("{}/registry", mockito::SERVER_URL)
        }
    } else {
        /// Returns the URL of the index of available Node versions on the public Node server.
        fn public_node_version_index() -> String {
            "https://nodejs.org/dist/index.json".to_string()
        }
        /// Return the URL of the index of available Yarn versions on the public git repository.
        fn public_yarn_version_index() -> String {
            "https://api.github.com/repos/yarnpkg/yarn/releases".to_string()
        }
        /// URL of the latest Yarn version on the public yarnpkg.com
        fn public_yarn_latest_version() -> String {
            "https://yarnpkg.com/latest-version".to_string()
        }
        /// URL of the Npm registry containing an index of availble public packages.
        fn public_package_registry_root() -> String {
            "https://registry.npmjs.org".to_string()
        }
    }
}

/// Lazily loaded inventory.
pub struct LazyInventory {
    inventory: LazyCell<Inventory>,
}

impl LazyInventory {
    /// Constructs a new `LazyInventory`.
    pub fn new() -> LazyInventory {
        LazyInventory {
            inventory: LazyCell::new(),
        }
    }

    /// Forces the loading of the inventory and returns an immutable reference to it.
    pub fn get(&self) -> Fallible<&Inventory> {
        self.inventory.try_borrow_with(|| Inventory::current())
    }

    /// Forces the loading of the inventory and returns a mutable reference to it.
    pub fn get_mut(&mut self) -> Fallible<&mut Inventory> {
        self.inventory.try_borrow_mut_with(|| Inventory::current())
    }
}

pub struct Collection<D: Distro> {
    // A sorted collection of the available versions in the inventory.
    pub versions: BTreeSet<Version>,

    pub phantom: PhantomData<D>,
}

pub type NodeCollection = Collection<NodeDistro>;
pub type YarnCollection = Collection<YarnDistro>;
pub type PackageCollection = Collection<PackageDistro>;

/// The inventory of locally available tool versions.
pub struct Inventory {
    pub node: NodeCollection,
    pub yarn: YarnCollection,
    pub packages: PackageCollection,
}

impl Inventory {
    /// Returns the current inventory.
    fn current() -> Fallible<Inventory> {
        Ok(Inventory {
            node: NodeCollection::load()?,
            yarn: YarnCollection::load()?,
            packages: PackageCollection::load()?,
        })
    }
}

impl<D: Distro> Collection<D> {
    /// Tests whether this Collection contains the specified Tool version.
    pub fn contains(&self, version: &Version) -> bool {
        self.versions.contains(version)
    }
}

pub trait FetchResolve<D: Distro> {
    type FetchedVersion;

    /// Fetch a Distro version matching the specified semantic versioning requirements.
    fn fetch(
        &mut self,
        name: &str, // unused by Node and Yarn, but package install needs this
        matching: &VersionSpec,
        hooks: Option<&ToolHooks<D>>,
    ) -> Fallible<Fetched<Self::FetchedVersion>>;

    /// Resolves the specified semantic versioning requirements into a distribution
    fn resolve(
        &self,
        name: &str,
        matching: &VersionSpec,
        hooks: Option<&ToolHooks<D>>,
    ) -> Fallible<D> {
        let version = match matching {
            VersionSpec::Latest => self.resolve_latest(&name, hooks)?,
            VersionSpec::Lts => self.resolve_lts(&name, hooks)?,
            VersionSpec::Semver(requirement) => self.resolve_semver(&name, requirement, hooks)?,
            VersionSpec::Exact(version) => self.resolve_exact(&name, version.to_owned(), hooks)?,
        };

        D::new(name, version, hooks)
    }

    /// Resolves the latest version for this tool, using either the `latest` hook or the public registry
    fn resolve_latest(
        &self,
        name: &str,
        hooks: Option<&ToolHooks<D>>,
    ) -> Fallible<D::ResolvedVersion>;

    /// Resolves a SemVer version for this tool, using either the `index` hook or the public registry
    fn resolve_semver(
        &self,
        name: &str,
        matching: &VersionReq,
        hooks: Option<&ToolHooks<D>>,
    ) -> Fallible<D::ResolvedVersion>;

    /// Resolves an LTS version for this tool
    fn resolve_lts(
        &self,
        name: &str,
        hooks: Option<&ToolHooks<D>>,
    ) -> Fallible<D::ResolvedVersion> {
        return self.resolve_latest(name, hooks);
    }

    /// Resolves an exact version of this tool
    fn resolve_exact(
        &self,
        name: &str,
        version: Version,
        hooks: Option<&ToolHooks<D>>,
    ) -> Fallible<D::ResolvedVersion>;
}

fn registry_fetch_error(
    tool: impl AsRef<str>,
    from_url: impl AsRef<str>,
) -> impl FnOnce(&reqwest::Error) -> ErrorDetails {
    let tool = tool.as_ref().to_string();
    let from_url = from_url.as_ref().to_string();
    |_| ErrorDetails::RegistryFetchError { tool, from_url }
}

fn match_node_version(
    url: &str,
    predicate: impl Fn(&NodeEntry) -> bool,
) -> Fallible<Option<Version>> {
    let index: NodeIndex = resolve_node_versions(url)?.into_index()?;
    let mut entries = index.entries.into_iter();
    Ok(entries
        .find(predicate)
        .map(|NodeEntry { version, .. }| version))
}

impl FetchResolve<NodeDistro> for NodeCollection {
    type FetchedVersion = NodeVersion;

    fn fetch(
        &mut self,
        name: &str, // not used here, we already know this is "node"
        matching: &VersionSpec,
        hooks: Option<&ToolHooks<NodeDistro>>,
    ) -> Fallible<Fetched<NodeVersion>> {
        let distro = self.resolve(name, matching, hooks)?;
        let fetched = distro.fetch(&self)?;

        if let &Fetched::Now(NodeVersion {
            runtime: ref version,
            ..
        }) = &fetched
        {
            self.versions.insert(version.clone());
        }

        Ok(fetched)
    }

    fn resolve_latest(
        &self,
        _name: &str,
        hooks: Option<&ToolHooks<NodeDistro>>,
    ) -> Fallible<Version> {
        // NOTE: This assumes the registry always produces a list in sorted order
        //       from newest to oldest. This should be specified as a requirement
        //       when we document the plugin API.
        let url = match hooks {
            Some(&ToolHooks {
                latest: Some(ref hook),
                ..
            }) => hook.resolve("index.json")?,
            _ => public_node_version_index(),
        };
        let version_opt = match_node_version(&url, |_| true)?;

        if let Some(version) = version_opt {
            Ok(version)
        } else {
            throw!(ErrorDetails::NodeVersionNotFound {
                matching: "latest".to_string()
            })
        }
    }

    fn resolve_semver(
        &self,
        _name: &str,
        matching: &VersionReq,
        hooks: Option<&ToolHooks<NodeDistro>>,
    ) -> Fallible<Version> {
        // ISSUE #34: also make sure this OS is available for this version
        let url = match hooks {
            Some(&ToolHooks {
                index: Some(ref hook),
                ..
            }) => hook.resolve("index.json")?,
            _ => public_node_version_index(),
        };
        let version_opt = match_node_version(&url, |&NodeEntry { version: ref v, .. }| {
            matching.matches(v)
        })?;

        if let Some(version) = version_opt {
            Ok(version)
        } else {
            throw!(ErrorDetails::NodeVersionNotFound {
                matching: matching.to_string()
            })
        }
    }

    fn resolve_lts(&self, _name: &str, hooks: Option<&ToolHooks<NodeDistro>>) -> Fallible<Version> {
        let url = match hooks {
            Some(&ToolHooks {
                index: Some(ref hook),
                ..
            }) => hook.resolve("index.json")?,
            _ => public_node_version_index(),
        };
        let version_opt = match_node_version(&url, |&NodeEntry { lts: val, .. }| val)?;

        if let Some(version) = version_opt {
            Ok(version)
        } else {
            throw!(ErrorDetails::NodeVersionNotFound {
                matching: "lts".to_string()
            })
        }
    }

    fn resolve_exact(
        &self,
        _name: &str,
        version: Version,
        _hooks: Option<&ToolHooks<NodeDistro>>,
    ) -> Fallible<Version> {
        Ok(version)
    }
}

impl FetchResolve<YarnDistro> for YarnCollection {
    type FetchedVersion = Version;

    /// Fetches a Yarn version matching the specified semantic versioning requirements.
    fn fetch(
        &mut self,
        name: &str, // not used here, we already know this is "yarn"
        matching: &VersionSpec,
        hooks: Option<&ToolHooks<YarnDistro>>,
    ) -> Fallible<Fetched<Self::FetchedVersion>> {
        let distro = self.resolve(name, &matching, hooks)?;
        let fetched = distro.fetch(&self)?;

        if let &Fetched::Now(ref version) = &fetched {
            self.versions.insert(version.clone());
        }

        Ok(fetched)
    }

    fn resolve_latest(
        &self,
        _name: &str,
        hooks: Option<&ToolHooks<YarnDistro>>,
    ) -> Fallible<Version> {
        let url = match hooks {
            Some(&ToolHooks {
                latest: Some(ref hook),
                ..
            }) => hook.resolve("latest-version")?,
            _ => public_yarn_latest_version(),
        };
        let mut response: reqwest::Response = reqwest::get(&url)
            .with_context(|_| ErrorDetails::YarnLatestFetchError { from_url: url })?;
        Version::parse(&response.text().unknown()?).unknown()
    }

    fn resolve_semver(
        &self,
        _name: &str,
        matching: &VersionReq,
        hooks: Option<&ToolHooks<YarnDistro>>,
    ) -> Fallible<Version> {
        let url = match hooks {
            Some(&ToolHooks {
                index: Some(ref hook),
                ..
            }) => hook.resolve("releases")?,
            _ => public_yarn_version_index(),
        };

        let spinner = progress_spinner(&format!("Fetching public registry: {}", url));
        let releases: serial::YarnIndex = reqwest::get(&url)
            .with_context(registry_fetch_error("Yarn", &url))?
            .json()
            .unknown()?;
        let releases = releases.into_index()?.entries;
        spinner.finish_and_clear();
        let version_opt = releases.into_iter().rev().find(|v| matching.matches(v));

        if let Some(version) = version_opt {
            Ok(version)
        } else {
            throw!(ErrorDetails::YarnVersionNotFound {
                matching: matching.to_string()
            })
        }
    }

    fn resolve_exact(
        &self,
        _name: &str,
        version: Version,
        _hooks: Option<&ToolHooks<YarnDistro>>,
    ) -> Fallible<Version> {
        Ok(version)
    }
}

// use the input predicate to match a package in the index
fn match_package_entry(
    index: PackageIndex,
    predicate: impl Fn(&PackageEntry) -> bool,
) -> Option<PackageEntry> {
    let mut entries = index.entries.into_iter();
    entries.find(predicate)
}

// fetch metadata for the input url
fn resolve_package_metadata(package_info_url: &str) -> Fallible<serial::PackageMetadata> {
    let spinner = progress_spinner(&format!("Fetching package metadata: {}", package_info_url));
    let mut response: reqwest::Response = reqwest::get(package_info_url).with_context(|_| {
        ErrorDetails::PackageMetadataFetchError {
            from_url: package_info_url.to_string(),
        }
    })?;
    let response_text: String = response.text().unknown()?;

    let metadata: serial::PackageMetadata = serde_json::de::from_str(&response_text).unknown()?;

    spinner.finish_and_clear();
    Ok(metadata)
}

impl FetchResolve<PackageDistro> for PackageCollection {
    type FetchedVersion = PackageVersion;

    /// Fetches a package version matching the specified semantic versioning requirements.
    fn fetch(
        &mut self,
        name: &str,
        matching: &VersionSpec,
        hooks: Option<&ToolHooks<PackageDistro>>,
    ) -> Fallible<Fetched<Self::FetchedVersion>> {
        let distro = self.resolve(name, &matching, hooks)?;
        let fetched = distro.fetch(&self)?;

        if let &Fetched::Now(PackageVersion { ref version, .. }) = &fetched {
            self.versions.insert(version.clone());
        }

        Ok(fetched)
    }

    fn resolve_latest(
        &self,
        name: &str,
        hooks: Option<&ToolHooks<PackageDistro>>,
    ) -> Fallible<PackageEntry> {
        let url = match hooks {
            Some(&ToolHooks {
                latest: Some(ref hook),
                ..
            }) => hook.resolve(&name)?,
            _ => format!("{}/{}", public_package_registry_root(), name),
        };

        let package_index = resolve_package_metadata(&url)?.into_index();
        let latest = package_index.latest.clone();

        let entry_opt =
            match_package_entry(package_index, |&PackageEntry { version: ref v, .. }| {
                &latest == v
            });

        if let Some(entry) = entry_opt {
            Ok(entry)
        } else {
            throw!(ErrorDetails::PackageVersionNotFound {
                name: name.to_string(),
                matching: String::from("latest"),
            })
        }
    }

    fn resolve_semver(
        &self,
        name: &str,
        matching: &VersionReq,
        hooks: Option<&ToolHooks<PackageDistro>>,
    ) -> Fallible<PackageEntry> {
        let url = match hooks {
            Some(&ToolHooks {
                index: Some(ref hook),
                ..
            }) => hook.resolve(&name)?,
            _ => format!("{}/{}", public_package_registry_root(), name),
        };

        let package_index = resolve_package_metadata(&url)?.into_index();

        let entry_opt =
            match_package_entry(package_index, |&PackageEntry { version: ref v, .. }| {
                matching.matches(v)
            });

        if let Some(entry) = entry_opt {
            Ok(entry)
        } else {
            throw!(ErrorDetails::PackageVersionNotFound {
                name: name.to_string(),
                matching: matching.to_string(),
            })
        }
    }

    fn resolve_exact(
        &self,
        name: &str,
        exact_version: Version,
        hooks: Option<&ToolHooks<PackageDistro>>,
    ) -> Fallible<PackageEntry> {
        let url = match hooks {
            Some(&ToolHooks {
                index: Some(ref hook),
                ..
            }) => hook.resolve(&name)?,
            _ => format!("{}/{}", public_package_registry_root(), name),
        };

        let package_index = resolve_package_metadata(&url)?.into_index();

        let entry_opt =
            match_package_entry(package_index, |&PackageEntry { version: ref v, .. }| {
                &exact_version == v
            });

        if let Some(entry) = entry_opt {
            Ok(entry)
        } else {
            throw!(ErrorDetails::PackageVersionNotFound {
                name: name.to_string(),
                matching: exact_version.to_string(),
            })
        }
    }
}

/// The index of the public Node server.
pub struct NodeIndex {
    entries: Vec<NodeEntry>,
}

#[derive(Debug)]
pub struct NodeEntry {
    pub version: Version,
    pub npm: Version,
    pub files: NodeDistroFiles,
    pub lts: bool,
}

/// The public Yarn index.
pub struct YarnIndex {
    entries: BTreeSet<Version>,
}

/// The set of available files on the public Node server for a given Node version.
#[derive(Debug)]
pub struct NodeDistroFiles {
    pub files: HashSet<String>,
}

/// Reads a public index from the Node cache, if it exists and hasn't expired.
fn read_cached_opt() -> Fallible<Option<serial::NodeIndex>> {
    let expiry: Option<String> = read_file_opt(&path::node_index_expiry_file()?).unknown()?;

    if let Some(string) = expiry {
        let expiry_date: HttpDate = HttpDate::from_str(&string).unknown()?;
        let current_date: HttpDate = HttpDate::from(SystemTime::now());

        if current_date < expiry_date {
            let cached: Option<String> = read_file_opt(&path::node_index_file()?).unknown()?;

            if let Some(string) = cached {
                return Ok(serde_json::de::from_str(&string).unknown()?);
            }
        }
    }

    Ok(None)
}

/// Get the cache max-age of an HTTP reponse.
fn max_age(response: &reqwest::Response) -> u32 {
    if let Some(cache_control_header) = response.headers().get_011::<CacheControl>() {
        for cache_directive in cache_control_header.iter() {
            if let CacheDirective::MaxAge(max_age) = cache_directive {
                return *max_age;
            }
        }
    }

    // Default to four hours.
    4 * 60 * 60
}

fn resolve_node_versions(url: &str) -> Fallible<serial::NodeIndex> {
    match read_cached_opt()? {
        Some(serial) => Ok(serial),
        None => {
            let spinner = progress_spinner(&format!("Fetching public registry: {}", url));
            let mut response: reqwest::Response =
                reqwest::get(url).with_context(registry_fetch_error("Node", url))?;
            let response_text: String = response.text().unknown()?;
            let cached: NamedTempFile = NamedTempFile::new_in(path::tmp_dir()?).unknown()?;

            // Block to borrow cached for cached_file.
            {
                let mut cached_file: &File = cached.as_file();
                cached_file.write(response_text.as_bytes()).unknown()?;
            }

            let index_cache_file = path::node_index_file()?;
            ensure_containing_dir_exists(&index_cache_file)?;
            cached.persist(index_cache_file).unknown()?;

            let expiry: NamedTempFile = NamedTempFile::new_in(path::tmp_dir()?).unknown()?;

            // Block to borrow expiry for expiry_file.
            {
                let mut expiry_file: &File = expiry.as_file();

                if let Some(expires_header) = response.headers().get_011::<Expires>() {
                    write!(expiry_file, "{}", expires_header).unknown()?;
                } else {
                    let expiry_date =
                        SystemTime::now() + Duration::from_secs(max_age(&response).into());

                    write!(expiry_file, "{}", HttpDate::from(expiry_date)).unknown()?;
                }
            }

            let index_expiry_file = path::node_index_expiry_file()?;
            ensure_containing_dir_exists(&index_expiry_file)?;
            expiry.persist(index_expiry_file).unknown()?;

            let serial: serial::NodeIndex = serde_json::de::from_str(&response_text).unknown()?;

            spinner.finish_and_clear();
            Ok(serial)
        }
    }
}
