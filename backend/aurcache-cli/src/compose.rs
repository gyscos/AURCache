//! Rendering `docker-compose.yaml` files for the shapes AURCache ships in.
//!
//! Templates rather than a YAML serializer, deliberately. The compose files in
//! the repository are two thirds comment — why the worker needs `privileged`,
//! what the shared `enroll` volume buys, which host name the TLS certificate
//! has to be valid for — and a serializer would strip every one of them and
//! reorder the keys for good measure. The comments are the part a first-time
//! user actually needs, so they are what the template preserves.
//!
//! The output mirrors `docker-compose.yaml` (bundle) and
//! `docker-compose.remote-worker.yaml` (worker) at the repository root. When
//! those change, these should follow.

use aurcache_common::ports::{AURCACHE_HTTP_PORT, AURCACHE_MIRROR_PORT, AURCACHE_WORKER_PORT};
use clap::ValueEnum;
use std::fmt::Write as _;

pub const SERVER_IMAGE: &str = "ghcr.io/lukas-heiligenbrunner/aurcache-server:latest";
pub const WORKER_IMAGE: &str = "ghcr.io/lukas-heiligenbrunner/aurcache-worker:latest";

/// Written into a generated worker file when the operator has not supplied a
/// fingerprint, so the file is obviously unfinished rather than quietly
/// trusting whatever answers.
pub const FINGERPRINT_PLACEHOLDER: &str = "REPLACE_WITH_SERVER_CA_SHA256";

/// The service name the server runs under, which is also the host name workers
/// dial inside the compose network — and therefore a name the server's TLS
/// certificate must be valid for.
const SERVER_SERVICE: &str = "aurcache";

/// Where the worker drops its enrollment request in the bundled setup.
pub const ENROLLMENT_DIR: &str = "/enroll";

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum ComposeRole {
    /// The server alone, for a host that gets its workers from elsewhere.
    Backend,
    /// A worker alone, joining a server that already exists.
    Worker,
    /// Server plus one local worker: the turnkey default.
    #[default]
    Bundle,
}

impl ComposeRole {
    /// The conventional file name for this role.
    #[must_use]
    pub const fn default_filename(self) -> &'static str {
        match self {
            Self::Backend => "docker-compose.backend.yaml",
            Self::Worker => "docker-compose.worker.yaml",
            Self::Bundle => "docker-compose.yaml",
        }
    }

    const fn has_server(self) -> bool {
        matches!(self, Self::Backend | Self::Bundle)
    }

    const fn has_worker(self) -> bool {
        matches!(self, Self::Worker | Self::Bundle)
    }
}

/// Every worker environment variable the CLI knows how to set.
///
/// One type, shared with the `docker run` path, so the variable names and their
/// formatting are decided once. A worker configured through a compose file and
/// the same worker configured through `setup worker` must not disagree about
/// what `WORKER_ARCHES` looks like.
#[derive(Debug, Clone, Default)]
pub struct WorkerEnv {
    pub url: Option<String>,
    pub ca_fingerprint: Option<String>,
    pub enrollment_token: Option<String>,
    pub enrollment_dir: Option<String>,
    pub name: Option<String>,
    pub arches: Vec<String>,
    pub emulated_arches: Vec<String>,
    pub packages: Vec<String>,
    pub concurrency: Option<u32>,
    pub priority: Option<i32>,
}

impl WorkerEnv {
    /// Only the variables that have a value, in a stable order.
    ///
    /// Unset means absent rather than empty: the worker has its own defaults —
    /// the host name, the host architecture — and an empty string would
    /// override them with nothing instead of leaving them alone.
    #[must_use]
    pub fn to_pairs(&self) -> Vec<(&'static str, String)> {
        let mut pairs = Vec::new();
        let mut push = |key, value: Option<String>| {
            if let Some(value) = value {
                pairs.push((key, value));
            }
        };

        push("AURCACHE_URL", self.url.clone());
        push(
            "AURCACHE_SERVER_CA_FINGERPRINT",
            self.ca_fingerprint.clone(),
        );
        push("AURCACHE_ENROLLMENT_TOKEN", self.enrollment_token.clone());
        push("AURCACHE_ENROLLMENT_DIR", self.enrollment_dir.clone());
        push("WORKER_NAME", self.name.clone());
        push("WORKER_ARCHES", join_list(&self.arches));
        push("WORKER_EMULATED_ARCHES", join_list(&self.emulated_arches));
        push("WORKER_PACKAGES", join_list(&self.packages));
        push(
            "WORKER_CONCURRENCY",
            self.concurrency.map(|v| v.to_string()),
        );
        push("WORKER_PRIORITY", self.priority.map(|v| v.to_string()));
        pairs
    }
}

/// A repeated flag becomes one comma-separated variable, which is what the
/// worker parses.
fn join_list(values: &[String]) -> Option<String> {
    (!values.is_empty()).then(|| values.join(","))
}

#[derive(Debug, Clone)]
pub struct ComposeParams {
    pub role: ComposeRole,
    pub server_image: String,
    pub worker_image: String,
    pub public_url: String,
    pub log_level: String,
    /// Host names the server's TLS certificate must be valid for. A worker
    /// dialing a name that is not in here fails the handshake.
    pub tls_sans: String,
    pub worker: WorkerEnv,
}

impl Default for ComposeParams {
    fn default() -> Self {
        Self {
            role: ComposeRole::default(),
            server_image: SERVER_IMAGE.to_string(),
            worker_image: WORKER_IMAGE.to_string(),
            public_url: format!("http://localhost:{AURCACHE_MIRROR_PORT}"),
            log_level: "info".to_string(),
            tls_sans: format!("{SERVER_SERVICE},localhost"),
            worker: WorkerEnv::default(),
        }
    }
}

#[must_use]
pub fn render_compose(params: &ComposeParams) -> String {
    let mut out = String::new();
    out.push_str(&header(params.role));
    out.push_str("services:\n");

    if params.role.has_server() {
        out.push_str(&server_service(params));
    }
    if params.role.has_worker() {
        out.push_str(&worker_service(params));
    }

    out.push_str(&volumes(params.role));
    if params.role.has_server() {
        // A single-service worker file needs no network: it reaches the server
        // over the host's, wherever that server happens to be.
        out.push_str("\nnetworks:\n  aurcache:\n    driver: bridge\n");
    }
    out
}

fn header(role: ComposeRole) -> String {
    let rule = "# =============================================================================\n";
    let body = match role {
        ComposeRole::Bundle => {
            "# AURCache — bundled single-host setup (the turnkey default)\n\
             # =============================================================================\n\
             # One command, zero edits, zero secrets, zero approval clicks:\n\
             #\n\
             #     docker compose up -d\n\
             #\n\
             # This starts the AURCache server plus a single local build worker. The worker\n\
             # auto-enrolls by dropping its certificate request into the shared `enroll`\n\
             # volume that the server reads — presence in that volume is proof of trust, so\n\
             # no token and no manual approval are needed.\n\
             #\n\
             # To scale local build throughput, raise WORKER_CONCURRENCY or run more workers:\n\
             #     docker compose up -d --scale builder=3\n"
        }
        ComposeRole::Backend => {
            "# AURCache — server only\n\
             # =============================================================================\n\
             # The server with no worker of its own: every builder joins from elsewhere.\n\
             # Nothing will build until at least one does.\n\
             #\n\
             #     docker compose up -d\n\
             #\n\
             # The `enroll` volume is still declared and read: a worker on this same host\n\
             # that mounts it is auto-approved. A worker on other hardware cannot, and is\n\
             # approved by fingerprint, by shared token, or once in the web UI.\n"
        }
        ComposeRole::Worker => {
            "# AURCache — build worker\n\
             # =============================================================================\n\
             # A worker joining an AURCache server that runs elsewhere. Jobs for this\n\
             # machine's native architecture are routed here in preference to emulation.\n\
             #\n\
             # There is no shared `enroll` volume across machines, so trust is explicit:\n\
             #\n\
             #   1. Anti-MITM — pin the server's CA fingerprint, printed in its startup log:\n\
             #          \"Worker CA fingerprint (pin this on workers): <sha256>\"\n\
             #      Without a pin the worker trusts on first use: fine on a trusted\n\
             #      network, not over the public internet.\n\
             #\n\
             #   2. Approving THIS worker — pick one: pre-approve its fingerprint with the\n\
             #      server's AURCACHE_PREAPPROVED_WORKERS, share an AURCACHE_ENROLLMENT_TOKEN,\n\
             #      or approve it once on the Workers page.\n\
             #\n\
             #     docker compose up -d\n"
        }
    };

    format!(
        "{rule}{body}#\n# Generated by `aurcache-cli setup compose`. Edit freely — it is a\n\
         # starting point, not a managed file.\n{rule}\n"
    )
}

fn server_service(params: &ComposeParams) -> String {
    let ComposeParams {
        server_image,
        public_url,
        log_level,
        tls_sans,
        ..
    } = params;

    format!(
        "  {SERVER_SERVICE}:\n\
         \x20   # Backend only. The `aurcache` image (without `-server`) is the hybrid\n\
         \x20   # compatibility image, which bundles its own worker — do not use it here.\n\
         \x20   image: {server_image}\n\
         \x20   ports:\n\
         \x20     - \"{AURCACHE_HTTP_PORT}:{AURCACHE_HTTP_PORT}\"   # Web UI + API (plain HTTP; front with a reverse proxy for TLS)\n\
         \x20     - \"{AURCACHE_MIRROR_PORT}:{AURCACHE_MIRROR_PORT}\"   # Pacman repository (plain HTTP, for `pacman -Sy`)\n\
         \x20     - \"{AURCACHE_WORKER_PORT}:{AURCACHE_WORKER_PORT}\"   # Remote-worker protocol (HTTPS + mutual TLS)\n\
         \x20   environment:\n\
         \x20     - LOG_LEVEL={log_level}\n\
         \x20     # The server's TLS certificate must be valid for the hostname each worker\n\
         \x20     # dials. In-compose that is the service name; keep `localhost` so the\n\
         \x20     # UI/API is reachable from the host too.\n\
         \x20     - AURCACHE_TLS_SANS={tls_sans}\n\
         \x20     # Public base URL pacman clients (and workers) use to fetch built\n\
         \x20     # packages. Set this to your host's address or domain for remote clients.\n\
         \x20     - AURCACHE_PUBLIC_URL={public_url}\n\
         \x20     # Auto-approve any worker that can write to the shared `enroll` volume,\n\
         \x20     # mounted read-only here. No secret, no approval click.\n\
         \x20     - AURCACHE_ENROLLMENT_DIR={ENROLLMENT_DIR}\n\
         \x20   volumes:\n\
         \x20     - aurcache_db:/app/db\n\
         \x20     - aurcache_repo:/app/repo\n\
         \x20     - aurcache_ca:/app/data/ca      # internal worker CA (persist across restarts)\n\
         \x20     - enroll:{ENROLLMENT_DIR}:ro             # read the workers' enrollment CSRs\n\
         \x20   networks:\n\
         \x20     - aurcache\n\
         \x20   restart: unless-stopped\n\n"
    )
}

fn worker_service(params: &ComposeParams) -> String {
    let bundled = params.role == ComposeRole::Bundle;
    let mut env = params.worker.clone();

    if bundled {
        // Inside the compose network the server is reachable by service name,
        // and the shared volume is what makes approval automatic.
        env.url
            .get_or_insert_with(|| format!("https://{SERVER_SERVICE}:{AURCACHE_WORKER_PORT}"));
        env.enrollment_dir
            .get_or_insert_with(|| ENROLLMENT_DIR.to_string());
        // A fingerprint pin is meaningless against a server in the same compose
        // project: the CA is created by that very server.
        env.ca_fingerprint = None;
    } else {
        env.url
            .get_or_insert_with(|| format!("https://aurcache.example.com:{AURCACHE_WORKER_PORT}"));
        env.ca_fingerprint
            .get_or_insert_with(|| FINGERPRINT_PLACEHOLDER.to_string());
    }

    let mut out = String::new();
    out.push_str("  builder:\n");
    let _ = writeln!(out, "    image: {}", params.worker_image);
    if bundled {
        out.push_str("    depends_on:\n      - aurcache\n");
    }

    out.push_str("    environment:\n");
    let _ = writeln!(out, "      - RUST_LOG={}", params.log_level);
    for (key, value) in env.to_pairs() {
        let _ = writeln!(out, "      - {key}={value}");
    }

    out.push_str("    volumes:\n");
    if bundled {
        out.push_str(
            "      - enroll:/enroll                # writable: drop CSR for auto-enrollment\n",
        );
    }
    out.push_str(
        "      - worker_data:/var/lib/aurcache-worker   # persisted base chroot\n\
         \x20     - worker_cache:/var/cache/aurcache-worker\n",
    );

    out.push_str(
        "    # devtools builds each package in a systemd-nspawn chroot, which needs the\n\
         \x20   # namespaces and mounts a plain container forbids. `privileged` plus a tmpfs\n\
         \x20   # `/run` is the tightest configuration that works out of the box.\n\
         \x20   privileged: true\n\
         \x20   tmpfs:\n\
         \x20     - /run\n",
    );
    if bundled {
        out.push_str("    networks:\n      - aurcache\n");
    }
    out.push_str("    restart: unless-stopped\n\n");
    out
}

fn volumes(role: ComposeRole) -> String {
    let mut out = String::from("volumes:\n");
    if role.has_server() {
        out.push_str("  aurcache_db:\n  aurcache_repo:\n  aurcache_ca:\n");
    }
    // The bundle needs it on both sides; the backend declares it so a worker on
    // this host can be added later without editing the server service.
    if role.has_server() || role == ComposeRole::Bundle {
        out.push_str("  enroll:\n");
    }
    if role.has_worker() {
        out.push_str("  worker_data:\n  worker_cache:\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        ComposeParams, ComposeRole, ENROLLMENT_DIR, FINGERPRINT_PLACEHOLDER, WorkerEnv,
        render_compose,
    };

    fn params(role: ComposeRole) -> ComposeParams {
        ComposeParams {
            role,
            ..ComposeParams::default()
        }
    }

    /// Unset must mean absent: the worker defaults to the host's name and
    /// architecture, and an empty value would override those with nothing.
    #[test]
    fn unset_worker_values_produce_no_variable() {
        assert!(WorkerEnv::default().to_pairs().is_empty());
    }

    #[test]
    fn repeated_values_become_one_comma_separated_variable() {
        let env = WorkerEnv {
            arches: vec!["x86_64".to_string(), "aarch64".to_string()],
            ..WorkerEnv::default()
        };
        let pairs = env.to_pairs();
        assert_eq!(pairs, vec![("WORKER_ARCHES", "x86_64,aarch64".to_string())]);
    }

    /// The file is built by string concatenation, so one bad indent would ship
    /// a compose file docker refuses. Structural `contains` assertions would
    /// not notice; parsing it does.
    #[test]
    fn every_rendered_role_is_valid_yaml() {
        for role in [
            ComposeRole::Backend,
            ComposeRole::Worker,
            ComposeRole::Bundle,
        ] {
            let rendered = render_compose(&params(role));
            yaml_rust2::YamlLoader::load_from_str(&rendered)
                .unwrap_or_else(|e| panic!("{role:?} is not valid YAML: {e}\n{rendered}"));
        }
    }

    #[test]
    fn the_backend_role_has_a_server_and_no_builder() {
        let rendered = render_compose(&params(ComposeRole::Backend));
        assert!(rendered.contains("aurcache-server:latest"), "{rendered}");
        assert!(!rendered.contains("builder:"), "{rendered}");
        assert!(!rendered.contains("privileged"), "{rendered}");
    }

    #[test]
    fn the_worker_role_has_a_builder_and_no_server() {
        let rendered = render_compose(&params(ComposeRole::Worker));
        assert!(rendered.contains("builder:"), "{rendered}");
        assert!(!rendered.contains("aurcache-server:latest"), "{rendered}");
        assert!(!rendered.contains("aurcache_db:"), "{rendered}");
    }

    /// A worker file with no fingerprint must look unfinished rather than
    /// silently trusting whatever answers on that address.
    #[test]
    fn a_worker_without_a_pin_gets_a_loud_placeholder() {
        let rendered = render_compose(&params(ComposeRole::Worker));
        assert!(rendered.contains(FINGERPRINT_PLACEHOLDER), "{rendered}");
    }

    #[test]
    fn a_supplied_fingerprint_replaces_the_placeholder() {
        let mut p = params(ComposeRole::Worker);
        p.worker.ca_fingerprint = Some("abc123".to_string());
        let rendered = render_compose(&p);
        assert!(
            rendered.contains("AURCACHE_SERVER_CA_FINGERPRINT=abc123"),
            "{rendered}"
        );
        assert!(!rendered.contains(FINGERPRINT_PLACEHOLDER), "{rendered}");
    }

    /// The whole point of the bundle: the shared volume is what makes approval
    /// automatic, so it has to be read-only on the server and writable on the
    /// worker.
    #[test]
    fn the_bundle_shares_the_enrollment_volume_both_ways() {
        let rendered = render_compose(&params(ComposeRole::Bundle));
        assert!(
            rendered.contains(&format!("enroll:{ENROLLMENT_DIR}:ro")),
            "{rendered}"
        );
        assert!(rendered.contains("- enroll:/enroll  "), "{rendered}");
        assert!(rendered.contains("privileged: true"), "{rendered}");
    }

    /// Pinning a CA against a server in the same compose project is
    /// meaningless — that server issued the CA.
    #[test]
    fn the_bundle_does_not_pin_a_fingerprint() {
        let mut p = params(ComposeRole::Bundle);
        p.worker.ca_fingerprint = Some("abc123".to_string());
        let rendered = render_compose(&p);
        assert!(
            !rendered.contains("AURCACHE_SERVER_CA_FINGERPRINT"),
            "{rendered}"
        );
    }

    /// The server dials by service name inside the network, and its
    /// certificate has to be valid for that name.
    #[test]
    fn the_bundle_worker_dials_the_service_name_covered_by_the_sans() {
        let rendered = render_compose(&params(ComposeRole::Bundle));
        assert!(
            rendered.contains("AURCACHE_URL=https://aurcache:8083"),
            "{rendered}"
        );
        assert!(
            rendered.contains("AURCACHE_TLS_SANS=aurcache,localhost"),
            "{rendered}"
        );
    }

    #[test]
    fn worker_knobs_reach_the_rendered_file() {
        let mut p = params(ComposeRole::Worker);
        p.worker.concurrency = Some(4);
        p.worker.name = Some("arm-box".to_string());
        p.worker.arches = vec!["aarch64".to_string()];
        let rendered = render_compose(&p);
        assert!(rendered.contains("WORKER_CONCURRENCY=4"), "{rendered}");
        assert!(rendered.contains("WORKER_NAME=arm-box"), "{rendered}");
        assert!(rendered.contains("WORKER_ARCHES=aarch64"), "{rendered}");
    }

    #[test]
    fn each_role_has_its_own_conventional_filename() {
        assert_eq!(
            ComposeRole::Bundle.default_filename(),
            "docker-compose.yaml"
        );
        assert_ne!(
            ComposeRole::Worker.default_filename(),
            ComposeRole::Backend.default_filename()
        );
    }
}
