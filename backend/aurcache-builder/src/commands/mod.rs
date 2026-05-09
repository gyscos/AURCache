use aurcache_db::packages::SourceData;
use std::path::Path;

pub const GIT_REPO_PATH: &str = "/tmp";

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn shell_join_args(args: &str) -> String {
    args.split_whitespace()
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

fn fetch_required_pgp_keys_cmd() -> &'static str {
    "pgp_keys=\"$(if [ -f .SRCINFO ]; then \
         sed -n 's/^[[:space:]]*validpgpkeys[[:space:]]*=[[:space:]]*//p' .SRCINFO; \
     else \
         makepkg --printsrcinfo | sed -n 's/^[[:space:]]*validpgpkeys[[:space:]]*=[[:space:]]*//p'; \
     fi)\" && \
     if [ -n \"$pgp_keys\" ]; then \
         while IFS= read -r key; do \
             [ -n \"$key\" ] || continue; \
             if ! gpg --batch --list-keys \"$key\" >/dev/null 2>&1; then \
                 gpg --batch --keyserver hkps://keyserver.ubuntu.com --recv-keys \"$key\"; \
             fi; \
         done <<< \"$pgp_keys\"; \
     fi"
}

pub fn build_build_command(
    source_data: &SourceData,
    pkgbase: &str,
    build_flags: &str,
    container_build_dir: &Path,
) -> String {
    let build_dir = shell_quote(&container_build_dir.display().to_string());
    let quoted_pkgbase = shell_quote(pkgbase);
    let quoted_build_flags = shell_join_args(build_flags);

    let self_update = "sudo pacman -Syu --noconfirm --noprogressbar --color never";

    // The source only affects how we get the pkgbuild.
    let get_source = match source_data {
        SourceData::Aur { .. } => {
            let rpc_url = std::env::var("AUR_RPC_URL")
                .unwrap_or_else(|_| "https://aur.archlinux.org/rpc/v5".to_string());
            let snapshot_url = aurcache_deps::snapshot_url(&rpc_url, pkgbase);
            format!(
                // Ideally we'd run bash with -e -o pipefail.
                "set -o pipefail && \
                 mkdir -p {build_dir} && cd {build_dir} && \
                 curl -fsSL {snapshot_url} | tar xz && \
                 cd {pkgbase}",
                build_dir = build_dir,
                snapshot_url = shell_quote(&snapshot_url),
                pkgbase = quoted_pkgbase,
            )
        }
        SourceData::Git { .. } => {
            let git_repo_path = shell_quote(GIT_REPO_PATH);
            format!("sudo chmod -R 1777 {git_repo_path} && cd {git_repo_path}")
        }
        SourceData::Upload { .. } => {
            todo!("unpack zip and store it in build container dir")
        }
    };

    // Then they all build the same.
    format!(
        "{self_update} && {get_source} && {fetch_pgp_keys} && makepkg -s {build_flags}",
        fetch_pgp_keys = fetch_required_pgp_keys_cmd(),
        build_flags = quoted_build_flags,
    )
}

pub fn wrap_with_makepkg_config(
    makepkg_config: &str,
    makepkg_config_path: &str,
    pacman_config: &str,
    build_cmd: &str,
) -> String {
    format!(
        "printf '%s' {makepkg_config} > {makepkg_config_path}\n\
         printf '%s' {pacman_config} | sudo tee /etc/pacman.conf >/dev/null\n\
         {build_cmd}",
        makepkg_config = shell_quote(makepkg_config),
        makepkg_config_path = shell_quote(makepkg_config_path),
        pacman_config = shell_quote(pacman_config),
    )
}

#[cfg(test)]
mod tests {
    use super::{GIT_REPO_PATH, build_build_command};
    use aurcache_db::packages::SourceData;
    use std::path::Path;

    #[test]
    fn aur_build_command_fetches_required_pgp_keys() {
        let cmd = build_build_command(
            &SourceData::Aur {
                name: "hello".to_string(),
            },
            "hello",
            "--noconfirm --noprogressbar --nocolor",
            Path::new("/build/src"),
        );

        assert!(cmd.contains("validpgpkeys"));
        assert!(cmd.contains("gpg --batch --keyserver hkps://keyserver.ubuntu.com --recv-keys"));
        assert!(cmd.contains("makepkg -s '--noconfirm' '--noprogressbar' '--nocolor'"));
        assert!(cmd.contains("curl -fsSL"));
    }

    #[test]
    fn git_build_command_fetches_required_pgp_keys() {
        let cmd = build_build_command(
            &SourceData::Git {
                spec: aurcache_db::packages::GitSourceSpec {
                    url: "https://example.test/repo.git".to_string(),
                    r#ref: "main".to_string(),
                    subfolder: ".".to_string(),
                },
            },
            "hello",
            "--noconfirm --noprogressbar --nocolor",
            Path::new("/build/src"),
        );

        assert!(cmd.contains(&format!("cd '{}'", GIT_REPO_PATH)));
        assert!(cmd.contains("validpgpkeys"));
        assert!(cmd.contains("gpg --batch --keyserver hkps://keyserver.ubuntu.com --recv-keys"));
    }
}
