use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use anyhow::Result;
use futures::future::Either;
use futures::{StreamExt, TryFutureExt};
use pep440_rs::Version;
use pep508_rs::{MarkerEnvironment, Requirement, VersionOrUrl};
use tracing::debug;

use puffin_client::{File, PypiClient, SimpleJson};
use puffin_package::metadata::Metadata21;
use puffin_package::package_name::PackageName;
use puffin_package::requirements::Requirements;
use puffin_package::wheel::WheelFilename;
use puffin_platform::tags::Tags;

#[derive(Debug)]
pub struct Resolution(HashMap<PackageName, PinnedPackage>);

impl Resolution {
    pub fn iter(&self) -> impl Iterator<Item = (&PackageName, &PinnedPackage)> {
        self.0.iter()
    }
}

#[derive(Debug)]
pub struct PinnedPackage {
    metadata: Metadata21,
    file: File,
}

impl PinnedPackage {
    pub fn filename(&self) -> &str {
        &self.file.filename
    }

    pub fn url(&self) -> &str {
        &self.file.url
    }

    pub fn version(&self) -> &Version {
        &self.metadata.version
    }
}

/// Resolve a set of requirements into a set of pinned versions.
pub async fn resolve(
    requirements: &Requirements,
    markers: &MarkerEnvironment,
    tags: &Tags,
    client: &PypiClient,
) -> Result<Resolution> {
    // A channel to fetch package metadata (e.g., given `flask`, fetch all versions) and version
    // metadata (e.g., given `flask==1.0.0`, fetch the metadata for that version).
    let (package_sink, package_stream) = futures::channel::mpsc::unbounded();

    // Initialize the package stream.
    let mut package_stream = package_stream
        .map(|request: Request| match request {
            Request::Package(requirement) => Either::Left(
                client
                    // TODO(charlie): Remove this clone.
                    .simple(requirement.name.clone())
                    .map_ok(move |metadata| Response::Package(requirement, metadata)),
            ),
            Request::Version(requirement, file) => Either::Right(
                client
                    // TODO(charlie): Remove this clone.
                    .file(file.clone())
                    .map_ok(move |metadata| Response::Version(requirement, file, metadata)),
            ),
        })
        .buffer_unordered(32)
        .ready_chunks(32);

    // Push all the requirements into the package sink.
    let mut in_flight: HashSet<PackageName> = HashSet::with_capacity(requirements.len());
    for requirement in requirements.iter() {
        debug!("--> adding root dependency: {}", requirement);
        package_sink.unbounded_send(Request::Package(requirement.clone()))?;
        in_flight.insert(PackageName::normalize(&requirement.name));
    }

    // Resolve the requirements.
    let mut resolution: HashMap<PackageName, PinnedPackage> =
        HashMap::with_capacity(requirements.len());

    while let Some(chunk) = package_stream.next().await {
        for result in chunk {
            let result: Response = result?;
            match result {
                Response::Package(requirement, metadata) => {
                    // TODO(charlie): Support URLs. Right now, we treat a URL as an unpinned dependency.
                    let specifiers =
                        requirement
                            .version_or_url
                            .as_ref()
                            .and_then(|version_or_url| match version_or_url {
                                VersionOrUrl::VersionSpecifier(specifiers) => Some(specifiers),
                                VersionOrUrl::Url(_) => None,
                            });

                    // Pick a version that satisfies the requirement.
                    let Some(file) = metadata.files.iter().rev().find(|file| {
                        // We only support wheels for now.
                        let Ok(name) = WheelFilename::from_str(file.filename.as_str()) else {
                            return false;
                        };

                        let Ok(version) = Version::from_str(&name.version) else {
                            return false;
                        };

                        if !name.is_compatible(tags) {
                            return false;
                        }

                        specifiers
                            .iter()
                            .all(|specifier| specifier.contains(&version))
                    }) else {
                        continue;
                    };

                    package_sink.unbounded_send(Request::Version(requirement, file.clone()))?;
                }
                Response::Version(requirement, file, metadata) => {
                    debug!(
                        "--> selected version {} for {}",
                        metadata.version, requirement
                    );

                    // Add to the resolved set.
                    let normalized_name = PackageName::normalize(&requirement.name);
                    in_flight.remove(&normalized_name);
                    resolution.insert(
                        normalized_name,
                        PinnedPackage {
                            // TODO(charlie): Remove this clone.
                            metadata: metadata.clone(),
                            file,
                        },
                    );

                    // Enqueue its dependencies.
                    for dependency in metadata.requires_dist {
                        if !dependency.evaluate_markers(
                            markers,
                            // TODO(charlie): Remove this clone.
                            requirement.extras.clone().unwrap_or_default(),
                        ) {
                            debug!("--> ignoring {dependency} due to environment mismatch");
                            continue;
                        }

                        let normalized_name = PackageName::normalize(&dependency.name);

                        if resolution.contains_key(&normalized_name) {
                            continue;
                        }

                        if !in_flight.insert(normalized_name) {
                            continue;
                        }

                        debug!("--> adding transitive dependency: {}", dependency);

                        package_sink.unbounded_send(Request::Package(dependency))?;
                    }
                }
            }
        }

        if in_flight.is_empty() {
            break;
        }
    }

    Ok(Resolution(resolution))
}

#[derive(Debug)]
enum Request {
    Package(Requirement),
    Version(Requirement, File),
}

#[derive(Debug)]
enum Response {
    Package(Requirement, SimpleJson),
    Version(Requirement, File, Metadata21),
}