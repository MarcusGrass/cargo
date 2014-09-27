use std::io::{mod, fs, File};
use std::io::fs::PathExtensions;
use std::collections::HashMap;

use curl::http;
use git2;
use flate2::reader::GzDecoder;
use serialize::json;
use serialize::hex::ToHex;
use tar::Archive;
use url::Url;

use core::{Source, SourceId, PackageId, Package, Summary, Registry};
use core::Dependency;
use sources::{PathSource, git};
use util::{CargoResult, Config, internal, ChainError, ToUrl, human};
use util::{hex, Require, Sha256};
use ops;

static CENTRAL: &'static str = "https://example.com";

pub struct RegistrySource<'a, 'b:'a> {
    source_id: SourceId,
    checkout_path: Path,
    cache_path: Path,
    src_path: Path,
    config: &'a mut Config<'b>,
    handle: Option<http::Handle>,
    sources: Vec<PathSource>,
    hashes: HashMap<(String, String), String>, // (name, vers) => cksum
}

#[deriving(Decodable)]
pub struct RegistryConfig {
    pub dl: String,
    pub api: String,
}

#[deriving(Decodable)]
struct RegistryPackage {
    name: String,
    vers: String,
    deps: Vec<RegistryDependency>,
    features: HashMap<String, Vec<String>>,
    cksum: String,
}

#[deriving(Decodable)]
struct RegistryDependency {
    name: String,
    req: String,
    features: Vec<String>,
    optional: bool,
    default_features: bool,
    target: Option<String>,
}

impl<'a, 'b> RegistrySource<'a, 'b> {
    pub fn new(source_id: &SourceId,
               config: &'a mut Config<'b>) -> RegistrySource<'a, 'b> {
        let hash = hex::short_hash(source_id);
        let ident = source_id.get_url().host().unwrap().to_string();
        let part = format!("{}-{}", ident, hash);
        RegistrySource {
            checkout_path: config.registry_index_path().join(part.as_slice()),
            cache_path: config.registry_cache_path().join(part.as_slice()),
            src_path: config.registry_source_path().join(part.as_slice()),
            config: config,
            source_id: source_id.clone(),
            handle: None,
            sources: Vec::new(),
            hashes: HashMap::new(),
        }
    }

    /// Get the configured default registry URL.
    ///
    /// This is the main cargo registry by default, but it can be overridden in
    /// a .cargo/config
    pub fn url() -> CargoResult<Url> {
        let config = try!(ops::registry_configuration());
        let url = config.index.unwrap_or(CENTRAL.to_string());
        url.as_slice().to_url().map_err(human)
    }

    /// Get the default url for the registry
    pub fn default_url() -> String {
        CENTRAL.to_string()
    }

    /// Decode the configuration stored within the registry.
    ///
    /// This requires that the index has been at least checked out.
    pub fn config(&self) -> CargoResult<RegistryConfig> {
        let mut f = try!(File::open(&self.checkout_path.join("config.json")));
        let contents = try!(f.read_to_string());
        let config = try!(json::decode(contents.as_slice()));
        Ok(config)
    }

    /// Open the git repository for the index of the registry.
    ///
    /// This will attempt to open an existing checkout, and failing that it will
    /// initialize a fresh new directory and git checkout. No remotes will be
    /// configured by default.
    fn open(&self) -> CargoResult<git2::Repository> {
        match git2::Repository::open(&self.checkout_path) {
            Ok(repo) => return Ok(repo),
            Err(..) => {}
        }

        try!(fs::mkdir_recursive(&self.checkout_path, io::USER_DIR));
        let _ = fs::rmdir_recursive(&self.checkout_path);
        let repo = try!(git2::Repository::init(&self.checkout_path));
        Ok(repo)
    }

    /// Download the given package from the given url into the local cache.
    ///
    /// This will perform the HTTP request to fetch the package. This function
    /// will only succeed if the HTTP download was successful and the file is
    /// then ready for inspection.
    ///
    /// No action is taken if the package is already downloaded.
    fn download_package(&mut self, pkg: &PackageId, url: &Url)
                        -> CargoResult<Path> {
        // TODO: should discover from the S3 redirect
        let filename = format!("{}-{}.tar.gz", pkg.get_name(), pkg.get_version());
        let dst = self.cache_path.join(filename);
        if dst.exists() { return Ok(dst) }
        try!(self.config.shell().status("Downloading", pkg));

        try!(fs::mkdir_recursive(&dst.dir_path(), io::USER_DIR));
        let handle = match self.handle {
            Some(ref mut handle) => handle,
            None => {
                self.handle = Some(try!(ops::http_handle()));
                self.handle.as_mut().unwrap()
            }
        };
        // TODO: don't download into memory (curl-rust doesn't expose it)
        let resp = try!(handle.get(url.to_string()).follow_redirects(true).exec());
        if resp.get_code() != 200 && resp.get_code() != 0 {
            return Err(internal(format!("Failed to get 200 reponse from {}\n{}",
                                        url, resp)))
        }

        // Verify what we just downloaded
        let expected = self.hashes.find(&(pkg.get_name().to_string(),
                                          pkg.get_version().to_string()));
        let expected = try!(expected.require(|| {
            internal(format!("no hash listed for {}", pkg))
        }));
        let actual = {
            let mut state = Sha256::new();
            state.update(resp.get_body());
            state.finish()
        };
        if actual.as_slice().to_hex() != *expected {
            return Err(human(format!("Failed to verify the checksum of `{}`",
                                     pkg)))
        }

        try!(File::create(&dst).write(resp.get_body()));
        Ok(dst)
    }

    /// Unpacks a downloaded package into a location where it's ready to be
    /// compiled.
    ///
    /// No action is taken if the source looks like it's already unpacked.
    fn unpack_package(&self, pkg: &PackageId, tarball: Path)
                      -> CargoResult<Path> {
        let dst = self.src_path.join(format!("{}-{}", pkg.get_name(),
                                             pkg.get_version()));
        if dst.join(".cargo-ok").exists() { return Ok(dst) }

        try!(fs::mkdir_recursive(&dst.dir_path(), io::USER_DIR));
        let f = try!(File::open(&tarball));
        let gz = try!(GzDecoder::new(f));
        let mut tar = Archive::new(gz);
        try!(tar.unpack(&dst.dir_path()));
        try!(File::create(&dst.join(".cargo-ok")));
        Ok(dst)
    }

    /// Parse a line from the registry's index file into a Summary for a
    /// package.
    fn parse_registry_package(&mut self, line: &str) -> CargoResult<Summary> {
        let RegistryPackage {
            name, vers, cksum, deps, features
        } = try!(json::decode::<RegistryPackage>(line));
        let pkgid = try!(PackageId::new(name.as_slice(),
                                        vers.as_slice(),
                                        &self.source_id));
        let deps: CargoResult<Vec<Dependency>> = deps.into_iter().map(|dep| {
            self.parse_registry_dependency(dep)
        }).collect();
        let deps = try!(deps);
        self.hashes.insert((name, vers), cksum);
        Summary::new(pkgid, deps, features)
    }

    /// Converts an encoded dependency in the registry to a cargo dependency
    fn parse_registry_dependency(&self, dep: RegistryDependency)
                                 -> CargoResult<Dependency> {
        let RegistryDependency {
            name, req, features, optional, default_features, target
        } = dep;

        let dep = try!(Dependency::parse(name.as_slice(), Some(req.as_slice()),
                                         &self.source_id));
        drop(target); // FIXME: pass this in
        Ok(dep.optional(optional)
              .default_features(default_features)
              .features(features))
    }
}

impl<'a, 'b> Registry for RegistrySource<'a, 'b> {
    fn query(&mut self, dep: &Dependency) -> CargoResult<Vec<Summary>> {
        let name = dep.get_name();
        let path = self.checkout_path.clone();
        let path = match name.len() {
            1 => path.join("1").join(name),
            2 => path.join("2").join(name),
            3 => path.join("3").join(name.slice_to(1)).join(name),
            _ => path.join(name.slice(0, 2))
                     .join(name.slice(2, 4))
                     .join(name),
        };
        let contents = match File::open(&path) {
            Ok(mut f) => try!(f.read_to_string()),
            Err(..) => return Ok(Vec::new()),
        };

        let ret: CargoResult<Vec<Summary>>;
        ret = contents.as_slice().lines().filter(|l| l.trim().len() > 0)
                      .map(|l| self.parse_registry_package(l))
                      .collect();
        let mut summaries = try!(ret.chain_error(|| {
            internal(format!("Failed to parse registry's information for: {}",
                             dep.get_name()))
        }));
        summaries.query(dep)
    }
}

impl<'a, 'b> Source for RegistrySource<'a, 'b> {
    fn update(&mut self) -> CargoResult<()> {
        try!(self.config.shell().status("Updating",
             format!("registry `{}`", self.source_id.get_url())));
        let repo = try!(self.open());

        // git fetch origin
        let url = self.source_id.get_url().to_string();
        let refspec = "refs/heads/*:refs/remotes/origin/*";
        try!(git::fetch(&repo, url.as_slice(), refspec).chain_error(|| {
            internal(format!("failed to fetch `{}`", url))
        }));

        // git reset --hard origin/master
        let reference = "refs/remotes/origin/master";
        let oid = try!(repo.refname_to_id(reference));
        log!(5, "[{}] updating to rev {}", self.source_id, oid);
        let object = try!(repo.find_object(oid, None));
        try!(repo.reset(&object, git2::Hard, None, None));
        Ok(())
    }

    fn download(&mut self, packages: &[PackageId]) -> CargoResult<()> {
        let config = try!(self.config());
        let url = try!(config.dl.as_slice().to_url().map_err(internal));
        for package in packages.iter() {
            if self.source_id != *package.get_source_id() { continue }

            let mut url = url.clone();
            url.path_mut().unwrap().push(package.get_name().to_string());
            url.path_mut().unwrap().push(package.get_version().to_string());
            url.path_mut().unwrap().push("download".to_string());
            let path = try!(self.download_package(package, &url).chain_error(|| {
                internal(format!("Failed to download package `{}` from {}",
                                 package, url))
            }));
            let path = try!(self.unpack_package(package, path).chain_error(|| {
                internal(format!("Failed to unpack package `{}`", package))
            }));
            let mut src = PathSource::new(&path, &self.source_id);
            try!(src.update());
            self.sources.push(src);
        }
        Ok(())
    }

    fn get(&self, packages: &[PackageId]) -> CargoResult<Vec<Package>> {
        let mut ret = Vec::new();
        for src in self.sources.iter() {
            ret.extend(try!(src.get(packages)).into_iter());
        }
        return Ok(ret);
    }

    fn fingerprint(&self, pkg: &Package) -> CargoResult<String> {
        Ok(pkg.get_package_id().get_version().to_string())
    }
}
