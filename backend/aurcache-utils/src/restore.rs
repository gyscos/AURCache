//! Reading a dump back in.
//!
//! The order here is the whole design, and it is not the obvious one.
//!
//! Every package row is inserted *before* any dependency is resolved. That
//! removes the question of what order to import in -- there is no order, so a
//! dependency cycle is not a special case -- but on its own it is not enough. A
//! local package satisfies a dependency by its pkgbase, by one of its split
//! package names, or by something it `provides`, and a dump carries only the
//! first: the other two are derived from the PKGBUILD and are deliberately not
//! exported.
//!
//! So a git-sourced package that provides `libfoo` would not be recognised as
//! satisfying a dependency on `libfoo`, and resolution would fall through to
//! the AUR and adopt an AUR package in its place -- silently replacing the
//! source the operator chose. Which is why every package's source is resolved,
//! filling in `provides` and its split package names, *before* the dependency
//! graph is rebuilt. By then every package the dump carried is a complete
//! candidate and nothing can fall through.
//!
//! Restore has to read those sources anyway: `dependencies` is not exported
//! either -- an imported graph could disagree with today's AUR -- so the depends
//! lists have to be rediscovered from the sources regardless.

use std::collections::BTreeMap;
use std::io::Read;

use aurcache_common::api::dump::{
    DUMP_SCHEMA_VERSION, DumpManifest, DumpPackages, DumpSettings, DumpWorker, MANIFEST_FILE,
    PACKAGES_FILE, PATCH_DIR, SETTINGS_FILE, WORKERS_FILE,
};
use flate2::read::GzDecoder;

/// A dump that has been read and checked, ready to apply.
#[derive(Debug)]
pub struct LoadedDump {
    pub manifest: DumpManifest,
    pub packages: DumpPackages,
    pub settings: DumpSettings,
    pub workers: Vec<DumpWorker>,
    pub patches: BTreeMap<String, String>,
}

/// Read and validate a `.tar.gz` dump.
///
/// Everything is checked before anything is applied. An import that fails on
/// package 90 of 200 leaves a database that is neither what it was nor what the
/// dump describes, which is worse than either.
pub fn load_dump(bytes: &[u8]) -> anyhow::Result<LoadedDump> {
    let mut files = BTreeMap::new();
    let mut archive = tar::Archive::new(GzDecoder::new(bytes));
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().into_owned();
        let mut content = String::new();
        // A dump holds JSON and patches; anything that is not text is not
        // something this format has ever contained.
        entry
            .read_to_string(&mut content)
            .map_err(|e| anyhow::anyhow!("{path} is not readable as text: {e}"))?;
        files.insert(path, content);
    }

    let manifest: DumpManifest = parse(&files, MANIFEST_FILE)?;
    // Checked before anything else is even parsed: a newer dump may describe
    // things this version has no idea how to apply, and guessing at them is how
    // a restore silently drops what it did not understand.
    if manifest.schema_version > DUMP_SCHEMA_VERSION {
        anyhow::bail!(
            "this dump is version {} but this AURCache understands up to {DUMP_SCHEMA_VERSION}; \
             upgrade AURCache to restore it",
            manifest.schema_version
        );
    }

    let packages: DumpPackages = parse(&files, PACKAGES_FILE)?;
    let settings: DumpSettings = parse(&files, SETTINGS_FILE)?;
    let workers: Vec<DumpWorker> = parse(&files, WORKERS_FILE)?;

    let mut patches = BTreeMap::new();
    for (path, content) in &files {
        if let Some(pkgbase) = path
            .strip_prefix(&format!("{PATCH_DIR}/"))
            .and_then(|name| name.strip_suffix(".patch"))
        {
            // A patch for a package the dump does not carry has nothing to
            // attach to. Refused rather than ignored: it means the archive was
            // assembled wrongly, and silently dropping it loses an edit someone
            // made deliberately.
            if !packages.contains_key(pkgbase) {
                anyhow::bail!("{path} has no matching package in {PACKAGES_FILE}");
            }
            patches.insert(pkgbase.to_string(), content.clone());
        }
    }

    // Settings naming a package the dump does not carry would have nowhere to
    // go, and would quietly not be applied.
    for pkgbase in settings.packages.keys() {
        if !packages.contains_key(pkgbase) {
            anyhow::bail!(
                "{SETTINGS_FILE} configures '{pkgbase}', which is not in {PACKAGES_FILE}"
            );
        }
    }

    for (pkgbase, package) in &packages {
        if package.platforms.is_empty() {
            anyhow::bail!("'{pkgbase}' lists no platforms, so nothing could build it");
        }
    }

    Ok(LoadedDump {
        manifest,
        packages,
        settings,
        workers,
        patches,
    })
}

fn parse<T: serde::de::DeserializeOwned>(
    files: &BTreeMap<String, String>,
    name: &str,
) -> anyhow::Result<T> {
    let content = files
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("{name} is missing; this is not an AURCache dump"))?;
    serde_json::from_str(content).map_err(|e| anyhow::anyhow!("{name} is not valid: {e}"))
}

#[cfg(test)]
mod tests {
    use super::load_dump;
    use aurcache_common::api::dump::DUMP_SCHEMA_VERSION;

    /// Build an archive from explicit file contents, so a test can write a
    /// malformed one as easily as a good one.
    fn archive(files: &[(&str, &str)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
                &mut bytes,
                flate2::Compression::default(),
            ));
            for (path, content) in files {
                let mut header = tar::Header::new_gnu();
                header.set_path(path).unwrap();
                header.set_size(content.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                tar.append(&header, content.as_bytes()).unwrap();
            }
            tar.finish().unwrap();
        }
        bytes
    }

    fn manifest(version: u32) -> String {
        format!(
            r#"{{"schema_version":{version},"aurcache_version":"t","created_at":0,"includes_secrets":false}}"#
        )
    }

    const ONE_PACKAGE: &str = r#"{"hello":{"source_data":{"type":"aur","name":"hello"},
        "platforms":["x86_64"],"build_flags":[],"directly_requested":true}}"#;

    fn good() -> Vec<(&'static str, String)> {
        vec![
            ("manifest.json", manifest(DUMP_SCHEMA_VERSION)),
            ("packages.json", ONE_PACKAGE.to_string()),
            (
                "settings.json",
                r#"{"global":{},"packages":{}}"#.to_string(),
            ),
            ("workers.json", "[]".to_string()),
        ]
    }

    fn load(files: Vec<(&str, String)>) -> anyhow::Result<super::LoadedDump> {
        let owned: Vec<(&str, &str)> = files.iter().map(|(a, b)| (*a, b.as_str())).collect();
        load_dump(&archive(&owned))
    }

    #[test]
    fn a_well_formed_dump_loads() {
        let loaded = load(good()).unwrap();
        assert!(loaded.packages.contains_key("hello"));
    }

    /// The manifest's version is the load-bearing part of the format: restoring
    /// into a *different* AURCache is the point, so one that cannot understand
    /// the file must say so rather than apply the half it recognises.
    #[test]
    fn a_newer_dump_is_refused() {
        let mut files = good();
        files[0] = ("manifest.json", manifest(DUMP_SCHEMA_VERSION + 1));
        let error = load(files).unwrap_err().to_string();
        assert!(error.contains("upgrade AURCache"), "unhelpful: {error}");
    }

    #[test]
    fn something_that_is_not_a_dump_is_refused() {
        let error = load_dump(&archive(&[("hello.txt", "hi")]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("not an AURCache dump"), "unhelpful: {error}");
    }

    /// A patch is attached by filename, so one naming a package that is not
    /// there was assembled wrongly. Dropping it silently would lose an edit
    /// somebody made on purpose.
    #[test]
    fn a_patch_without_its_package_is_refused() {
        let mut files = good();
        files.push(("patches/ghost.patch", "--- a\n".to_string()));
        let error = load(files).unwrap_err().to_string();
        assert!(error.contains("no matching package"), "unhelpful: {error}");
    }

    /// Same reasoning for a setting: it would have nowhere to go.
    #[test]
    fn a_setting_for_an_absent_package_is_refused() {
        let mut files = good();
        files[2] = (
            "settings.json",
            r#"{"global":{},"packages":{"ghost":{"k":"v"}}}"#.to_string(),
        );
        let error = load(files).unwrap_err().to_string();
        assert!(error.contains("not in packages.json"), "unhelpful: {error}");
    }

    /// A package with no platforms could never build, so importing it would
    /// produce a row that sits there doing nothing.
    #[test]
    fn a_package_with_no_platforms_is_refused() {
        let mut files = good();
        files[1] = (
            "packages.json",
            r#"{"hello":{"source_data":{"type":"aur","name":"hello"},
                "platforms":[],"build_flags":[],"directly_requested":true}}"#
                .to_string(),
        );
        let error = load(files).unwrap_err().to_string();
        assert!(error.contains("no platforms"), "unhelpful: {error}");
    }

    /// A patch reaches the package it names.
    #[test]
    fn a_patch_is_matched_to_its_package() {
        let mut files = good();
        files.push(("patches/hello.patch", "--- a\n+++ b\n".to_string()));
        let loaded = load(files).unwrap();
        assert_eq!(
            loaded.patches.get("hello").map(String::as_str),
            Some("--- a\n+++ b\n")
        );
    }
}
