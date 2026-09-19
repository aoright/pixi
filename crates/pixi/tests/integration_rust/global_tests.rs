use crate::common::{PixiControl, workspaces_dir};
use crate::setup_tracing;
use pixi_build_backend_passthrough::{BackendEvent, ObservableBackend, PassthroughBackend};
use pixi_build_frontend::BackendOverride;

/// Test that force-reinstall triggers rebuilding the package
#[tokio::test]
async fn test_source_package_with_passthrough_backend_for_global() {
    setup_tracing();

    // Create an observable backend and get the observer
    let (instantiator, mut observer) =
        ObservableBackend::instantiator(PassthroughBackend::instantiator());

    // Create a PixiControl instance with ObservableBackend
    let backend_override = BackendOverride::from_memory(instantiator);
    let pixi = PixiControl::new()
        .unwrap()
        .with_backend_override(backend_override);

    let root_dir = workspaces_dir()
        .join("source-backends")
        .join("source-package");

    // First install - should trigger conda_build_v1
    pixi.global_install()
        .with_path(root_dir.to_string_lossy())
        .await
        .unwrap();

    // Verify that conda_build_v1 was called
    let events = observer.events();
    assert!(events.contains(&BackendEvent::CondaBuildV1Called));

    // Second install - should NOT trigger conda_build_v1 (package is cached)
    pixi.global_install()
        .with_path(root_dir.to_string_lossy())
        .await
        .unwrap();

    // Verify that conda_build_v1 was *NOT* called
    let events = observer.events();
    assert!(!events.contains(&BackendEvent::CondaBuildV1Called));

    // Third install with force-reinstall - should trigger conda_build_v1 again
    pixi.global_install()
        .with_path(root_dir.to_string_lossy())
        .with_force_reinstall(true)
        .await
        .unwrap();

    // Verify that conda_build_v1 was called again
    let events = observer.events();
    assert!(events.contains(&BackendEvent::CondaBuildV1Called));
}

fn create_test_archive(
    output_path: &std::path::Path,
    name: &str,
    version: &str,
    build: &str,
    bin_name: &str,
    bin_content: &str,
) {
    use rattler_conda_types::compression_level::CompressionLevel;
    use rattler_conda_types::package::{IndexJson, PathsEntry, PathsJson};
    use rattler_conda_types::{NoArchType, PackageName, Platform};
    use std::path::PathBuf;

    let staging_dir = tempfile::tempdir().unwrap();
    let info_dir = staging_dir.path().join("info");
    let bin_dir = if cfg!(windows) {
        staging_dir.path().join("Scripts")
    } else {
        staging_dir.path().join("bin")
    };
    fs_err::create_dir_all(&info_dir).unwrap();
    fs_err::create_dir_all(&bin_dir).unwrap();

    let bin_file_name = if cfg!(windows) {
        format!("{bin_name}.bat")
    } else {
        bin_name.to_string()
    };
    let bin_path = bin_dir.join(&bin_file_name);
    fs_err::write(&bin_path, bin_content).unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs_err::metadata(&bin_path).unwrap().permissions();
        perms.set_mode(0o755);
        fs_err::set_permissions(&bin_path, perms).unwrap();
    }

    let bin_bytes = bin_content.as_bytes();
    let bin_sha256 = rattler_digest::compute_bytes_digest::<rattler_digest::Sha256>(bin_bytes);

    let index_json = IndexJson {
        arch: None,
        build: build.to_string(),
        build_number: 0,
        constrains: vec![],
        depends: vec![],
        extra_depends: Default::default(),
        features: None,
        license: None,
        license_family: None,
        name: PackageName::new_unchecked(name),
        noarch: NoArchType::none(),
        platform: None,
        purls: None,
        python_site_packages_path: None,
        subdir: Some(Platform::current().to_string()),
        timestamp: None,
        track_features: vec![],
        version: version.parse().unwrap(),
        flags: vec![],
        repodata_revision: None,
    };
    let index_json_content = serde_json::to_string_pretty(&index_json).unwrap();
    let index_json_path = info_dir.join("index.json");
    fs_err::write(&index_json_path, &index_json_content).unwrap();

    let index_bytes = index_json_content.as_bytes();
    let index_sha256 = rattler_digest::compute_bytes_digest::<rattler_digest::Sha256>(index_bytes);

    let rel_bin_path = if cfg!(windows) {
        PathBuf::from("Scripts").join(&bin_file_name)
    } else {
        PathBuf::from("bin").join(&bin_file_name)
    };

    let paths_json = PathsJson {
        paths: vec![
            PathsEntry {
                relative_path: PathBuf::from("info/index.json"),
                no_link: false,
                path_type: rattler_conda_types::package::PathType::HardLink,
                prefix_placeholder: None,
                sha256: Some(index_sha256),
                size_in_bytes: Some(index_bytes.len() as u64),
            },
            PathsEntry {
                relative_path: rel_bin_path,
                no_link: false,
                path_type: rattler_conda_types::package::PathType::HardLink,
                prefix_placeholder: None,
                sha256: Some(bin_sha256),
                size_in_bytes: Some(bin_bytes.len() as u64),
            },
        ],
        paths_version: 1,
    };
    let paths_json_content = serde_json::to_string_pretty(&paths_json).unwrap();
    let paths_json_path = info_dir.join("paths.json");
    fs_err::write(&paths_json_path, &paths_json_content).unwrap();

    let paths = vec![
        info_dir.join("index.json"),
        info_dir.join("paths.json"),
        bin_path,
    ];

    let output_file = fs_err::File::create(output_path).unwrap();
    let out_name = format!("{name}-{version}-{build}");
    rattler_package_streaming::write::write_conda_package(
        output_file,
        staging_dir.path(),
        &paths,
        CompressionLevel::Default,
        None,
        &out_name,
        None,
        None,
    )
    .unwrap();
}

/// Test that installing an updated archive file with the same name/version/build
/// replaces the cached extraction when the sha256 changes (issue #6807).
#[tokio::test]
async fn test_global_install_path_replaces_cached_hash_mismatch() {
    setup_tracing();

    let temp_dir = tempfile::tempdir().unwrap();
    let pkg1_dir = temp_dir.path().join("pkg1");
    let pkg2_dir = temp_dir.path().join("pkg2");
    fs_err::create_dir_all(&pkg1_dir).unwrap();
    fs_err::create_dir_all(&pkg2_dir).unwrap();

    let pkg_name = "test-archive-pkg";
    let pkg_file_name = format!("{pkg_name}-0.1.0-0.conda");
    let pkg1_path = pkg1_dir.join(&pkg_file_name);
    let pkg2_path = pkg2_dir.join(&pkg_file_name);

    create_test_archive(
        &pkg1_path,
        pkg_name,
        "0.1.0",
        "0",
        "tool",
        "#!/bin/sh\necho v1\n",
    );
    create_test_archive(
        &pkg2_path,
        pkg_name,
        "0.1.0",
        "0",
        "tool",
        "#!/bin/sh\necho v2\n",
    );

    let channel_dir = tempfile::tempdir().unwrap();
    pixi_test_utils::MockRepoData::default()
        .write_repodata(channel_dir.path())
        .await
        .unwrap();

    let pixi = PixiControl::new().unwrap();

    // First install: install pkg1
    pixi.global_install()
        .with_local_channel(channel_dir.path())
        .with_path(pkg1_path.to_string_lossy())
        .await
        .unwrap();

    let installed_bin = pixi
        .workspace_path()
        .join("envs")
        .join(pkg_name)
        .join(if cfg!(windows) { "Scripts" } else { "bin" })
        .join(if cfg!(windows) { "tool.bat" } else { "tool" });

    assert_eq!(
        fs_err::read_to_string(&installed_bin).unwrap(),
        "#!/bin/sh\necho v1\n"
    );

    // Second install: install pkg2 with force-reinstall.
    // Even though name, version, and build match, sha256 differs.
    // The installer must extract pkg2 and not reuse pkg1 from cache.
    pixi.global_install()
        .with_local_channel(channel_dir.path())
        .with_path(pkg2_path.to_string_lossy())
        .with_force_reinstall(true)
        .await
        .unwrap();

    assert_eq!(
        fs_err::read_to_string(&installed_bin).unwrap(),
        "#!/bin/sh\necho v2\n"
    );
}
