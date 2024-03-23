/*!
This module handles the calls to Docker needed to execute package and variant
builds. The actual build steps and the expected parameters are defined in
the repository's top-level Dockerfile.

*/
pub(crate) mod error;

use crate::args::{BuildPackageArgs, BuildType, BuildVariantArgs};
use buildsys::manifest::{
    ImageFeature, ImageFormat, ImageLayout, ManifestInfo, PartitionPlan, SupportedArch,
};
use duct::cmd;
use error::Result;
use lazy_static::lazy_static;
use nonzero_ext::nonzero;
use pipesys::server::Server as PipesysServer;
use rand::Rng;
use regex::Regex;
use sha2::{Digest, Sha512};
use snafu::{ensure, OptionExt, ResultExt};
use std::collections::HashSet;
use std::env;
use std::fs::{self, read_dir, File};
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::process::Output;
use walkdir::{DirEntry, WalkDir};

/*
There's a bug in BuildKit that can lead to a build failure during parallel
`docker build` executions:
   https://github.com/moby/buildkit/issues/1090

Unfortunately we can't do much to control the concurrency here, and even when
the bug is fixed there will be many older versions of Docker in the wild.

The failure has an exit code of 1, which is too generic to be helpful. All we
can do is check the output for the error's signature, and retry if we find it.
*/
lazy_static! {
    static ref DOCKER_BUILD_FRONTEND_ERROR: Regex = Regex::new(concat!(
        r#"failed to solve with frontend dockerfile.v0: "#,
        r#"failed to solve with frontend gateway.v0: "#,
        r#"frontend grpc server closed unexpectedly"#
    ))
    .unwrap();
}

/*
There's a similar bug that's fixed in new releases of BuildKit but still in the wild in popular
versions of Docker/BuildKit:
   https://github.com/moby/buildkit/issues/1468
*/
lazy_static! {
    static ref DOCKER_BUILD_DEAD_RECORD_ERROR: Regex = Regex::new(concat!(
        r#"failed to solve with frontend dockerfile.v0: "#,
        r#"failed to solve with frontend gateway.v0: "#,
        r#"rpc error: code = Unknown desc = failed to build LLB: "#,
        r#"failed to get dead record"#,
    ))
    .unwrap();
}

/*
We also see sporadic CI failures with only this error message.
We use (?m) for multi-line mode so we can match the message on a line of its own without splitting
the output ourselves; we match the regexes against the whole of stdout.
*/
lazy_static! {
    static ref UNEXPECTED_EOF_ERROR: Regex = Regex::new("(?m)unexpected EOF$").unwrap();
}

/*
Sometimes new RPMs are not fully written to the host directory before another build starts, which
exposes `createrepo_c` to partially-written RPMs that cannot be added to the repo metadata. Retry
these errors by restarting the build since the alternatives are to ignore the `createrepo_c` exit
code (masking other problems) or aggressively `sync()` the host directory (hurting performance).
*/
lazy_static! {
    static ref CREATEREPO_C_READ_HEADER_ERROR: Regex = Regex::new(&regex::escape(
        r#"C_CREATEREPOLIB: Warning: read_header: rpmReadPackageFile() error"#
    ))
    .unwrap();
}

static DOCKER_BUILD_MAX_ATTEMPTS: NonZeroU16 = nonzero!(10u16);

// Expected UID for unprivileged processes inside the build container.
const BUILDER_UID: u32 = 1000;

// `cargo` passes the jobserver file descriptors through this environment variable.
const CARGO_MAKEFLAGS: &str = "CARGO_MAKEFLAGS";

struct CommonBuildArgs {
    arch: SupportedArch,
    sdk: String,
    nocache: String,
    token: String,
    jobs_socket: String,
}

impl CommonBuildArgs {
    fn new(root: impl AsRef<Path>, sdk: String, arch: SupportedArch) -> Self {
        let token = token(&root);

        // Avoid using a cached layer from a previous build.
        let nocache = rand::thread_rng().gen::<u32>().to_string();

        // Generate a unique address for the socket that sends jobserver file descriptors.
        let jobs_socket = format!("buildsys-jobserver-{token}-{nocache}");

        Self {
            arch,
            sdk,
            nocache,
            token,
            jobs_socket,
        }
    }
}

struct PackageBuildArgs {
    /// The package might need to know what the `image_features` are going to be for the variant
    /// it is going to be used in downstream. This is because certain packages will be built
    /// differently based on certain image features such as cgroupsv1 vs cgroupsv2. During a
    /// package build, these are determined by looking at the variant's Cargo.toml file based on
    /// what was found in `BUILDSYS_VARIANT`.
    image_features: HashSet<ImageFeature>,
    package: String,
    publish_repo: String,
    variant: String,
    variant_family: String,
    variant_flavor: String,
    variant_platform: String,
    variant_runtime: String,
}

impl PackageBuildArgs {
    fn build_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        args.build_arg("PACKAGE", &self.package);
        args.build_arg("REPO", &self.publish_repo);
        args.build_arg("VARIANT", &self.variant);
        args.build_arg("VARIANT_FAMILY", &self.variant_family);
        args.build_arg("VARIANT_FLAVOR", &self.variant_flavor);
        args.build_arg("VARIANT_PLATFORM", &self.variant_platform);
        args.build_arg("VARIANT_RUNTIME", &self.variant_runtime);
        for image_feature in &self.image_features {
            args.build_arg(format!("{}", image_feature), "1");
        }

        args
    }
}

struct VariantBuildArgs {
    data_image_publish_size_gib: i32,
    data_image_size_gib: String,
    image_features: HashSet<ImageFeature>,
    image_format: String,
    kernel_parameters: String,
    name: String,
    os_image_publish_size_gib: String,
    os_image_size_gib: String,
    packages: String,
    partition_plan: String,
    pretty_name: String,
    variant: String,
    variant_family: String,
    variant_flavor: String,
    variant_platform: String,
    variant_runtime: String,
    version_build: String,
    version_image: String,
}

impl VariantBuildArgs {
    fn build_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        args.build_arg(
            "DATA_IMAGE_PUBLISH_SIZE_GIB",
            self.data_image_publish_size_gib.to_string(),
        );
        args.build_arg("DATA_IMAGE_SIZE_GIB", &self.data_image_size_gib);
        args.build_arg("IMAGE_FORMAT", &self.image_format);
        args.build_arg("KERNEL_PARAMETERS", &self.kernel_parameters);
        args.build_arg("IMAGE_NAME", &self.name);
        args.build_arg("OS_IMAGE_PUBLISH_SIZE_GIB", &self.os_image_publish_size_gib);
        args.build_arg("OS_IMAGE_SIZE_GIB", &self.os_image_size_gib);
        args.build_arg("PACKAGES", &self.packages);
        args.build_arg("PARTITION_PLAN", &self.partition_plan);
        args.build_arg("PRETTY_NAME", &self.pretty_name);
        args.build_arg("VARIANT", &self.variant);
        args.build_arg("VARIANT_FAMILY", &self.variant_family);
        args.build_arg("VARIANT_FLAVOR", &self.variant_flavor);
        args.build_arg("VARIANT_PLATFORM", &self.variant_platform);
        args.build_arg("VARIANT_RUNTIME", &self.variant_runtime);
        args.build_arg("BUILD_ID", &self.version_build);
        args.build_arg("VERSION_ID", &self.version_image);

        for image_feature in self.image_features.iter() {
            args.build_arg(format!("{}", image_feature), "1");
        }

        args
    }
}

#[allow(clippy::large_enum_variant)]
enum TargetBuildArgs {
    Package(PackageBuildArgs),
    Variant(VariantBuildArgs),
}

impl TargetBuildArgs {
    pub(crate) fn build_type(&self) -> BuildType {
        match self {
            TargetBuildArgs::Package(_) => BuildType::Package,
            TargetBuildArgs::Variant(_) => BuildType::Variant,
        }
    }
}

pub(crate) struct DockerBuild {
    dockerfile: PathBuf,
    context: PathBuf,
    target: String,
    tag: String,
    root_dir: PathBuf,
    artifacts_dir: PathBuf,
    state_dir: PathBuf,
    artifact_name: String,
    common_build_args: CommonBuildArgs,
    target_build_args: TargetBuildArgs,
    secrets_args: Vec<String>,
}

impl DockerBuild {
    /// Create a new `DockerBuild` that can build a package.
    pub(crate) fn new_package(
        args: BuildPackageArgs,
        manifest: &ManifestInfo,
        image_features: HashSet<ImageFeature>,
    ) -> Result<Self> {
        let package = if let Some(name_override) = manifest.package_name() {
            name_override.clone()
        } else {
            args.cargo_package_name
        };

        Ok(Self {
            dockerfile: args.common.tools_dir.join("Dockerfile"),
            context: args.common.root_dir.clone(),
            target: "package".to_string(),
            tag: append_token(
                format!(
                    "buildsys-pkg-{package}-{arch}",
                    package = package,
                    arch = args.common.arch,
                ),
                &args.common.root_dir,
            ),
            root_dir: args.common.root_dir.clone(),
            artifacts_dir: args.packages_dir,
            state_dir: args.common.state_dir,
            artifact_name: package.clone(),
            common_build_args: CommonBuildArgs::new(
                &args.common.root_dir,
                args.common.sdk_image,
                args.common.arch,
            ),
            target_build_args: TargetBuildArgs::Package(PackageBuildArgs {
                image_features,
                package,
                publish_repo: args.publish_repo,
                variant: args.variant,
                variant_family: args.variant_family,
                variant_flavor: args.variant_flavor,
                variant_platform: args.variant_platform,
                variant_runtime: args.variant_runtime,
            }),
            secrets_args: Vec::new(),
        })
    }

    /// Create a new `DockerBuild` that can build a variant image.
    pub(crate) fn new_variant(args: BuildVariantArgs, manifest: &ManifestInfo) -> Result<Self> {
        let image_layout = manifest.image_layout().cloned().unwrap_or_default();
        let ImageLayout {
            os_image_size_gib,
            data_image_size_gib,
            partition_plan,
            ..
        } = image_layout;

        let (os_image_publish_size_gib, data_image_publish_size_gib) =
            image_layout.publish_image_sizes_gib();

        Ok(Self {
            dockerfile: args.common.tools_dir.join("Dockerfile"),
            context: args.common.root_dir.clone(),
            target: "variant".to_string(),
            tag: append_token(
                format!(
                    "buildsys-var-{variant}-{arch}",
                    variant = args.variant,
                    arch = args.common.arch
                ),
                &args.common.root_dir,
            ),
            root_dir: args.common.root_dir.clone(),
            artifacts_dir: args.common.image_arch_variant_dir,
            state_dir: args.common.state_dir,
            artifact_name: args.variant.clone(),
            common_build_args: CommonBuildArgs::new(
                &args.common.root_dir,
                args.common.sdk_image,
                args.common.arch,
            ),
            target_build_args: TargetBuildArgs::Variant(VariantBuildArgs {
                data_image_publish_size_gib,
                data_image_size_gib: data_image_size_gib.to_string(),
                image_features: manifest.image_features().unwrap_or_default(),
                image_format: match manifest.image_format() {
                    Some(ImageFormat::Raw) | None => "raw",
                    Some(ImageFormat::Qcow2) => "qcow2",
                    Some(ImageFormat::Vmdk) => "vmdk",
                }
                .to_string(),
                kernel_parameters: manifest
                    .kernel_parameters()
                    .cloned()
                    .unwrap_or_default()
                    .join(" "),
                name: args.name,
                os_image_publish_size_gib: os_image_publish_size_gib.to_string(),
                os_image_size_gib: os_image_size_gib.to_string(),
                packages: manifest
                    .included_packages()
                    .cloned()
                    .unwrap_or_default()
                    .join(" "),
                partition_plan: match partition_plan {
                    PartitionPlan::Split => "split",
                    PartitionPlan::Unified => "unified",
                }
                .to_string(),
                pretty_name: args.pretty_name,
                variant: args.variant,
                variant_family: args.variant_family,
                variant_flavor: args.variant_flavor,
                variant_platform: args.variant_platform,
                variant_runtime: args.variant_runtime,
                version_build: args.version_build,
                version_image: args.version_image,
            }),
            secrets_args: secrets_args()?,
        })
    }

    pub(crate) fn build(&self) -> Result<()> {
        env::set_current_dir(&self.root_dir).context(error::DirectoryChangeSnafu {
            path: &self.root_dir,
        })?;

        // Create a directory for tracking outputs before we move them into position.
        let marker_dir = create_marker_dir(
            &self.target_build_args.build_type(),
            &self.artifact_name,
            &self.common_build_args.arch.to_string(),
            &self.state_dir,
        )?;

        // Clean up any previous outputs we have tracked.
        clean_build_files(&marker_dir, &self.artifacts_dir)?;

        let mut build = format!(
            "build {context} \
            --target {target} \
            --tag {tag} \
            --network host \
            --file {dockerfile} \
            --build-arg BYPASS_SOCKET={tag}-bypass",
            context = self.context.display(),
            dockerfile = self.dockerfile.display(),
            target = self.target,
            tag = self.tag,
        )
        .split_string();

        build.extend(self.build_args());
        build.extend(self.secrets_args.clone());

        // Run a container with the project's root as a read-only volume mount, so that pipesys can
        // serve a read-only file descriptor that's safe to pass into builds.
        let run_bypass = format!(
            "run \
            --name {tag}-bypass \
            --rm \
            --init \
            --net host \
            --pid host \
            -u 0 \
            -v {root}:/bypass:ro \
            -v {root}/build/tools/pipesys:/usr/local/bin/pipesys:ro \
            {sdk} \
            pipesys serve --socket {tag}-bypass --client-uid 0 --path /bypass",
            tag = self.tag,
            root = self.root_dir.display(),
            sdk = self.common_build_args.sdk,
        )
        .split_string();

        let rm_bypass = format!("rm --force {}-bypass", self.tag).split_string();

        // Helper inputs for the build container.
        let create = format!("create --name {} {} true", self.tag, self.tag).split_string();
        let cp = format!("cp {}:/output/. {}", self.tag, marker_dir.display()).split_string();
        let rm = format!("rm --force {}", self.tag).split_string();
        let rmi = format!("rmi --force {}", self.tag).split_string();

        // Clean up the stopped bypass container if it exists.
        let _ = docker(&rm_bypass, Retry::No);

        // Clean up the stopped build container if it exists.
        let _ = docker(&rm, Retry::No);

        // Clean up the previous image if it exists.
        let _ = docker(&rmi, Retry::No);

        // Get the jobserver file descriptors for pipesys to serve.
        let cargo_makeflags = env::var(CARGO_MAKEFLAGS).context(error::EnvironmentSnafu {
            var: CARGO_MAKEFLAGS,
        })?;
        let (read_fd, write_fd) = parse_makeflags(cargo_makeflags)?;
        let jobs_socket = self.common_build_args.jobs_socket.clone();

        let runtime = tokio::runtime::Runtime::new().context(error::AsyncRuntimeSnafu)?;

        // Spawn a background task to share the file descriptors for cargo's jobserver.
        runtime.spawn(async move {
            PipesysServer::for_fds(jobs_socket, BUILDER_UID, &[read_fd, write_fd])
                .serve()
                .await
        });

        // Spawn a background task for the bypass container that will serve the project root file
        // descriptor.
        runtime.spawn(async move {
            let _ = docker(&run_bypass, Retry::No);
        });

        // Build the image, which builds the artifacts we want.
        // Work around transient, known failure cases with Docker.
        let build_result = docker(
            &build,
            Retry::Yes {
                attempts: DOCKER_BUILD_MAX_ATTEMPTS,
                messages: &[
                    &*DOCKER_BUILD_FRONTEND_ERROR,
                    &*DOCKER_BUILD_DEAD_RECORD_ERROR,
                    &*UNEXPECTED_EOF_ERROR,
                    &*CREATEREPO_C_READ_HEADER_ERROR,
                ],
            },
        );

        // Clean up our bypass container.
        let _ = docker(&rm_bypass, Retry::No);

        // Stop the runtime and the background threads.
        runtime.shutdown_background();

        // Check whether the build succeeded before continuing.
        build_result?;

        // Create a stopped container so we can copy artifacts out.
        docker(&create, Retry::No)?;

        // Copy artifacts into our output directory.
        docker(&cp, Retry::No)?;

        // Clean up our stopped container after copying artifacts out.
        docker(&rm, Retry::No)?;

        // Clean up our image now that we're done.
        docker(&rmi, Retry::No)?;

        // Copy artifacts to the expected directory and write markers to track them.
        copy_build_files(&marker_dir, &self.artifacts_dir)?;

        Ok(())
    }

    fn build_args(&self) -> Vec<String> {
        let mut args = match &self.target_build_args {
            TargetBuildArgs::Package(p) => p.build_args(),
            TargetBuildArgs::Variant(v) => v.build_args(),
        };
        args.build_arg("ARCH", self.common_build_args.arch.to_string());
        args.build_arg("GOARCH", self.common_build_args.arch.goarch());
        args.build_arg("SDK", &self.common_build_args.sdk);
        args.build_arg("NOCACHE", &self.common_build_args.nocache);
        args.build_arg("TOKEN", &self.common_build_args.token);
        args.build_arg("JOBS_SOCKET", &self.common_build_args.jobs_socket);
        args
    }
}

// =^..^=   =^..^=   =^..^=   =^..^=   =^..^=   =^..^=   =^..^=   =^..^=   =^..^=   =^..^=   =^..^=

/// Run `docker` with the specified arguments.
fn docker(args: &[String], retry: Retry) -> Result<Output> {
    let mut max_attempts: u16 = 1;
    let mut retry_messages: &[&Regex] = &[];
    if let Retry::Yes { attempts, messages } = retry {
        max_attempts = attempts.into();
        retry_messages = messages;
    }

    let mut attempt = 1;
    loop {
        let output = cmd("docker", args)
            .stderr_to_stdout()
            .stdout_capture()
            .unchecked()
            .run()
            .context(error::CommandStartSnafu)?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        println!("{}", &stdout);
        if output.status.success() {
            return Ok(output);
        }

        ensure!(
            retry_messages.iter().any(|m| m.is_match(&stdout)) && attempt < max_attempts,
            error::DockerExecutionSnafu {
                args: &args.join(" ")
            }
        );

        attempt += 1;
    }
}

/// Allow the caller to configure retry behavior, since the command may fail
/// for spurious reasons that should not be treated as an error.
enum Retry<'a> {
    No,
    Yes {
        attempts: NonZeroU16,
        messages: &'a [&'static Regex],
    },
}

// =^..^=   =^..^=   =^..^=   =^..^=   =^..^=   =^..^=   =^..^=   =^..^=   =^..^=   =^..^=   =^..^=

/// Add secrets that might be needed for builds. Since most builds won't use
/// them, they are not automatically tracked for changes. If necessary, builds
/// can emit the relevant cargo directives for tracking in their build script.
fn secrets_args() -> Result<Vec<String>> {
    let mut args = Vec::new();
    let sbkeys_var = "BUILDSYS_SBKEYS_PROFILE_DIR";
    let sbkeys_dir = env::var(sbkeys_var).context(error::EnvironmentSnafu { var: sbkeys_var })?;

    let sbkeys = read_dir(&sbkeys_dir).context(error::DirectoryReadSnafu { path: &sbkeys_dir })?;
    for s in sbkeys {
        let s = s.context(error::DirectoryReadSnafu { path: &sbkeys_dir })?;
        args.build_secret(
            "file",
            &s.file_name().to_string_lossy(),
            &s.path().to_string_lossy(),
        );
    }

    for var in &[
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
    ] {
        let id = format!("{}.env", var.to_lowercase().replace('_', "-"));
        args.build_secret("env", &id, var);
    }

    Ok(args)
}

// =^..^=   =^..^=   =^..^=   =^..^=   =^..^=   =^..^=   =^..^=   =^..^=   =^..^=   =^..^=   =^..^=

/// Create a directory for build artifacts.
fn create_marker_dir(
    kind: &BuildType,
    name: &str,
    arch: &str,
    state_dir: &Path,
) -> Result<PathBuf> {
    let prefix = match kind {
        BuildType::Package => "packages",
        BuildType::Variant => "variants",
    };

    let path = [&state_dir.display().to_string(), arch, prefix, name]
        .iter()
        .collect();

    fs::create_dir_all(&path).context(error::DirectoryCreateSnafu { path: &path })?;

    Ok(path)
}

const MARKER_EXTENSION: &str = ".buildsys_marker";

/// Copy build artifacts to the output directory.
/// Before we copy each file, we create a corresponding marker file to record its existence.
fn copy_build_files<P>(build_dir: P, output_dir: P) -> Result<()>
where
    P: AsRef<Path>,
{
    fn has_artifacts(entry: &DirEntry) -> bool {
        let is_dir = entry.path().is_dir();
        let is_file = entry.file_type().is_file();
        let is_not_marker = is_file
            && entry
                .file_name()
                .to_str()
                .map(|s| !s.ends_with(MARKER_EXTENSION))
                .unwrap_or(false);
        let is_symlink = entry.file_type().is_symlink();
        is_dir || is_not_marker || is_symlink
    }

    for artifact_file in find_files(&build_dir, has_artifacts) {
        let mut marker_file = artifact_file.clone().into_os_string();
        marker_file.push(MARKER_EXTENSION);
        File::create(&marker_file).context(error::FileCreateSnafu { path: &marker_file })?;

        let mut output_file: PathBuf = output_dir.as_ref().into();
        output_file.push(artifact_file.strip_prefix(&build_dir).context(
            error::StripPathPrefixSnafu {
                path: &marker_file,
                prefix: build_dir.as_ref(),
            },
        )?);

        let parent_dir = output_file
            .parent()
            .context(error::BadDirectorySnafu { path: &output_file })?;
        fs::create_dir_all(parent_dir)
            .context(error::DirectoryCreateSnafu { path: &parent_dir })?;

        fs::rename(&artifact_file, &output_file).context(error::FileRenameSnafu {
            old_path: &artifact_file,
            new_path: &output_file,
        })?;
    }

    Ok(())
}

/// Remove build artifacts from the output directory.
/// Any marker file we find could have a corresponding file that should be cleaned up.
/// We also clean up the marker files so they do not accumulate across builds.
/// For the same reason, if a directory is empty after build artifacts, marker files, and other
/// empty directories have been removed, then that directory will also be removed.
fn clean_build_files<P>(build_dir: P, output_dir: P) -> Result<()>
where
    P: AsRef<Path>,
{
    let build_dir = build_dir.as_ref();
    let output_dir = output_dir.as_ref();

    fn has_markers(entry: &DirEntry) -> bool {
        let is_dir = entry.path().is_dir();
        let is_file = entry.file_type().is_file();
        let is_marker = is_file
            && entry
                .file_name()
                .to_str()
                .map(|s| s.ends_with(MARKER_EXTENSION))
                .unwrap_or(false);
        is_dir || is_marker
    }

    fn cleanup(path: &Path, top: &Path, dirs: &mut HashSet<PathBuf>) -> Result<()> {
        if !path.exists() && !path.is_symlink() {
            return Ok(());
        }
        std::fs::remove_file(path).context(error::FileRemoveSnafu { path })?;
        let mut parent = path.parent();
        while let Some(p) = parent {
            if p == top || dirs.contains(p) {
                break;
            }
            dirs.insert(p.into());
            parent = p.parent()
        }
        Ok(())
    }

    fn is_empty_dir(path: &Path) -> Result<bool> {
        Ok(path.is_dir()
            && path
                .read_dir()
                .context(error::DirectoryReadSnafu { path })?
                .next()
                .is_none())
    }

    let mut clean_dirs: HashSet<PathBuf> = HashSet::new();

    for marker_file in find_files(&build_dir, has_markers) {
        let mut output_file: PathBuf = output_dir.into();
        output_file.push(marker_file.strip_prefix(build_dir).context(
            error::StripPathPrefixSnafu {
                path: &marker_file,
                prefix: build_dir,
            },
        )?);
        output_file.set_extension("");
        cleanup(&output_file, output_dir, &mut clean_dirs)?;
        cleanup(&marker_file, build_dir, &mut clean_dirs)?;
    }

    // Clean up directories in reverse order, so that empty child directories don't stop an
    // otherwise empty parent directory from being removed.
    let mut clean_dirs = clean_dirs.into_iter().collect::<Vec<PathBuf>>();
    clean_dirs.sort_by(|a, b| b.cmp(a));

    for clean_dir in clean_dirs {
        if is_empty_dir(&clean_dir)? {
            std::fs::remove_dir(&clean_dir)
                .context(error::DirectoryRemoveSnafu { path: &clean_dir })?;
        }
    }

    Ok(())
}

/// Create an iterator over files matching the supplied filter.
fn find_files<P>(
    dir: P,
    filter: for<'r> fn(&'r walkdir::DirEntry) -> bool,
) -> impl Iterator<Item = PathBuf>
where
    P: AsRef<Path>,
{
    WalkDir::new(&dir)
        .follow_links(false)
        .same_file_system(true)
        .min_depth(1)
        .into_iter()
        .filter_entry(filter)
        .flat_map(|e| e.context(error::DirectoryWalkSnafu))
        .map(|e| e.into_path())
        .filter(|e| e.is_file() || e.is_symlink())
}

// =^..^=   =^..^=   =^..^=   =^..^=   =^..^=   =^..^=   =^..^=   =^..^=   =^..^=   =^..^=   =^..^=

/// Compute a per-checkout suffix for the tag to avoid collisions.
fn token(p: impl AsRef<Path>) -> String {
    let mut d = Sha512::new();
    d.update(p.as_ref().display().to_string());
    let digest = hex::encode(d.finalize());
    digest[..12].to_string()
}

/// Append the per-checkout suffix token to a Docker tag.
fn append_token(tag: impl AsRef<str>, p: impl AsRef<Path>) -> String {
    format!("{}-{}", tag.as_ref(), token(p))
}

/// Helper trait for constructing buildkit --build-arg arguments.
trait BuildArg {
    fn build_arg<S1, S2>(&mut self, key: S1, value: S2)
    where
        S1: AsRef<str>,
        S2: AsRef<str>;
}

impl BuildArg for Vec<String> {
    fn build_arg<S1, S2>(&mut self, key: S1, value: S2)
    where
        S1: AsRef<str>,
        S2: AsRef<str>,
    {
        self.push("--build-arg".to_string());
        self.push(format!("{}={}", key.as_ref(), value.as_ref()));
    }
}

/// Helper trait for constructing buildkit --secret arguments.
trait BuildSecret {
    fn build_secret<S>(&mut self, typ: S, id: S, src: S)
    where
        S: AsRef<str>;
}

impl BuildSecret for Vec<String> {
    fn build_secret<S>(&mut self, typ: S, id: S, src: S)
    where
        S: AsRef<str>,
    {
        self.push("--secret".to_string());
        self.push(format!(
            "type={},id={},src={}",
            typ.as_ref(),
            id.as_ref(),
            src.as_ref()
        ));
    }
}

/// Helper trait for splitting a string on spaces into owned Strings.
///
/// If you need an element with internal spaces, you should handle that separately, for example
/// with BuildArg.
trait SplitString {
    fn split_string(&self) -> Vec<String>;
}

impl<S> SplitString for S
where
    S: AsRef<str>,
{
    fn split_string(&self) -> Vec<String> {
        self.as_ref().split(' ').map(String::from).collect()
    }
}

// =^..^=   =^..^=   =^..^=   =^..^=   =^..^=   =^..^=   =^..^=   =^..^=   =^..^=   =^..^=   =^..^=

lazy_static! {
    static ref MAKEFLAGS: Regex = Regex::new(
        "^-j \
         --jobserver-fds=(?<read_fd>[0-9]+),(?<write_fd>[0-9]+) \
         --jobserver-auth=(?<auth_read_fd>[0-9]+),(?<auth_write_fd>[0-9]+)$"
    )
    .unwrap();
}

/// Helper function for parsing file descriptors from `CARGO_MAKEFLAGS`.
fn parse_makeflags<S>(input: S) -> Result<(i32, i32)>
where
    S: AsRef<str> + std::fmt::Display,
{
    let captures = MAKEFLAGS
        .captures(input.as_ref())
        .context(error::RegexMatchSnafu {
            input: input.to_string(),
            regex: MAKEFLAGS.to_string(),
        })?;
    let read_fd = &captures["read_fd"];
    let write_fd = &captures["write_fd"];
    let auth_read_fd = &captures["auth_read_fd"];
    let auth_write_fd = &captures["auth_write_fd"];

    ensure!(
        read_fd == auth_read_fd,
        error::FileDescriptorMismatchSnafu {
            what: "read",
            expected: read_fd,
            actual: auth_read_fd,
        }
    );

    ensure!(
        write_fd == auth_write_fd,
        error::FileDescriptorMismatchSnafu {
            what: "write",
            expected: write_fd,
            actual: auth_write_fd,
        }
    );

    let read_fd = read_fd
        .parse::<i32>()
        .context(error::FileDescriptorParseSnafu { input: read_fd })?;

    let write_fd = write_fd
        .parse::<i32>()
        .context(error::FileDescriptorParseSnafu { input: write_fd })?;

    Ok((read_fd, write_fd))
}

#[cfg(test)]
macro_rules! assert_error {
    ($result:expr, $error:ident) => {
        match $result {
            Err(error::Error::$error { .. }) => (),
            _ => panic!("Did not encounter expected error."),
        };
    };
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn makeflags_valid() {
        let result = parse_makeflags("-j --jobserver-fds=3,4 --jobserver-auth=3,4");
        let (read_fd, write_fd) = result.unwrap();
        assert_eq!(read_fd, 3);
        assert_eq!(write_fd, 4);
    }

    #[test]
    fn makeflags_empty() {
        let result = parse_makeflags("");
        assert_error!(result, RegexMatch);
    }

    #[test]
    fn makeflags_mismatched() {
        let result = parse_makeflags("-j --jobserver-fds=3,4 --jobserver-auth=5,4");
        assert_error!(result, FileDescriptorMismatch);

        let result = parse_makeflags("-j --jobserver-fds=3,4 --jobserver-auth=3,5");
        assert_error!(result, FileDescriptorMismatch);
    }

    #[test]
    fn makeflags_out_of_range() {
        let fd = u64::MAX;

        let input = format!("-j --jobserver-fds={fd},4 --jobserver-auth={fd},4");
        let result = parse_makeflags(input);
        assert_error!(result, FileDescriptorParse);

        let input = format!("-j --jobserver-fds=3,{fd} --jobserver-auth=3,{fd}");
        let result = parse_makeflags(input);
        assert_error!(result, FileDescriptorParse);
    }
}
