use std::path::Path;

use alpm_srcinfo::SourceInfoV1;

/// Fix known-erroneous source URLs in PKGBUILD or .SRCINFO content that
/// makepkg accepts but `alpm-srcinfo` rejects.
///
/// Currently only fixes `?signed/` → `?signed` (trailing slash after the
/// signed flag in git VCS source URLs). Only affects libaegis and h2o-git.
pub fn fix_source_urls(content: &str) -> String {
    content.replace("?signed/", "?signed")
}

/// Parse a PKGBUILD file, applying workarounds for common issues found in
/// real-world PKGBUILDs that makepkg accepts but `alpm-srcinfo` rejects.
pub fn parse_pkgbuild(path: &Path) -> anyhow::Result<SourceInfoV1> {
    if let Ok(info) = SourceInfoV1::from_pkgbuild(path) {
        return Ok(info);
    }

    let raw = std::fs::read_to_string(path)?;
    let fixed = fix_source_urls(&raw);
    if fixed == raw {
        anyhow::bail!("PKGBUILD parsing failed and no fixes were applied");
    }

    let dir = tempfile::tempdir()?;
    let fixed_path = dir.path().join("PKGBUILD");
    std::fs::write(&fixed_path, &fixed)?;
    let result = SourceInfoV1::from_pkgbuild(&fixed_path)?;
    dir.close()?;
    Ok(result)
}

/// Parse PKGBUILD content held in memory, applying the same workarounds as
/// [`parse_pkgbuild`]. Since `alpm-srcinfo` shells out to a bash script that
/// needs a real filesystem path to `source` the PKGBUILD, the content is
/// written to a short-lived temp file that's removed again as soon as
/// parsing finishes - no other part of this function touches disk.
pub fn parse_pkgbuild_content(content: &str) -> anyhow::Result<SourceInfoV1> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("PKGBUILD");

    std::fs::write(&path, content)?;
    let result = SourceInfoV1::from_pkgbuild(&path);
    let result = match result {
        Ok(info) => Ok(info),
        Err(_) => {
            let fixed = fix_source_urls(content);
            if fixed == content {
                anyhow::bail!("PKGBUILD parsing failed and no fixes were applied");
            }
            std::fs::write(&path, &fixed)?;
            SourceInfoV1::from_pkgbuild(&path).map_err(anyhow::Error::from)
        }
    };

    dir.close()?;
    result
}
