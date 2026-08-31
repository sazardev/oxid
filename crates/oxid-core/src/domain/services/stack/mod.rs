//! Recognising what a repository is built with, and how to containerise it.
//!
//! Until now Oxid needed a `Dockerfile` to do anything at all: a `NestJS` or
//! Vite repository without one was refused outright. That is the wrong
//! demand to make of the person this product is for — a team lead adding
//! preview environments to an existing service should not have to become a
//! Docker author first, and the Dockerfile they would write under pressure
//! is usually the one that ships a 1.2GB image with `node_modules` and the
//! source tree inside it.
//!
//! So this reads what a repository already says about itself. Every
//! ecosystem records its own runtime, dependencies and entry point in a
//! manifest that is checked in and kept accurate because the developer's own
//! tooling depends on it: `package.json`, `go.mod`, `pyproject.toml`,
//! `Cargo.toml`. Those are better evidence than anything Oxid could ask for
//! separately, and they cannot drift, because the project would break first.
//!
//! Deliberately pure. Detection takes a [`RepoManifest`] — a description of
//! which files exist and what the interesting ones contain — rather than a
//! path, so every rule here is testable without a filesystem, a network or
//! Docker, and the adapter's only job is reading files.
//!
//! Two rules run through all of it:
//!
//! - **A checked-in `Dockerfile` always wins.** Detection never overrides
//!   what someone wrote by hand; it fills the gap where there is nothing.
//! - **A guess is labelled as one.** What was detected, and how confident it
//!   is, travels with the environment so a wrong guess is visible in the
//!   dashboard rather than mysterious at build time.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// What a repository looks like from the outside: the paths it contains and
/// the contents of the manifests worth reading.
///
/// The adapter fills this in; nothing here touches a disk.
#[derive(Debug, Clone, Default)]
pub struct RepoManifest {
    /// Every path in the repository root, relative and slash-separated.
    /// Only the root is needed — every ecosystem puts its manifest there.
    pub entries: Vec<String>,
    /// Contents of files detection actually reads, by the same relative
    /// path. Absent means "not read" and is treated as "not present".
    pub files: BTreeMap<String, String>,
}

impl RepoManifest {
    #[must_use]
    pub fn has(&self, path: &str) -> bool {
        self.entries.iter().any(|e| e == path)
    }

    #[must_use]
    pub fn read(&self, path: &str) -> Option<&str> {
        self.files.get(path).map(String::as_str)
    }

    /// Paths detection wants the contents of. The adapter reads these and
    /// nothing else — keeping the list here means the domain decides what
    /// is worth opening, and a repository is never trawled.
    #[must_use]
    pub fn files_worth_reading() -> &'static [&'static str] {
        &[
            "package.json",
            ".nvmrc",
            "go.mod",
            "pyproject.toml",
            "requirements.txt",
            "Cargo.toml",
            // Workspace declarations. Which directories hold packages is
            // stated in exactly one of these, and guessing at `apps/` and
            // `packages/` instead would be wrong for every repository that
            // named them something else.
            "pnpm-workspace.yaml",
            "turbo.json",
            "nx.json",
            "lerna.json",
        ]
    }

    /// The directories a workspace's members are conventionally under.
    ///
    /// Used only to bound what the adapter walks: the authoritative list of
    /// members comes from the workspace declaration, and this is what stops
    /// a repository being trawled to find it.
    #[must_use]
    pub fn workspace_roots() -> &'static [&'static str] {
        &["apps", "packages", "services", "libs"]
    }

    /// A member's own `package.json`, by workspace-relative directory.
    #[must_use]
    pub fn member_package(&self, dir: &str) -> Option<&str> {
        self.read(&format!("{dir}/package.json"))
    }
}

/// The language runtime a repository targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Runtime {
    Node,
    Go,
    Python,
    Rust,
    /// Nothing to run: HTML and assets, served as files.
    Static,
}

impl Runtime {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Go => "go",
            Self::Python => "python",
            Self::Rust => "rust",
            Self::Static => "static",
        }
    }
}

/// How the Node ecosystem's dependencies get installed.
///
/// Taken from the lockfile, which is the only honest source: a `packageManager`
/// field can lie, and installing with the wrong one either fails or silently
/// resolves different versions than the developer tested against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageManager {
    Npm,
    Pnpm,
    Yarn,
    Bun,
}

impl PackageManager {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Pnpm => "pnpm",
            Self::Yarn => "yarn",
            Self::Bun => "bun",
        }
    }

    /// The install command for a build, which is not the one a developer
    /// runs.
    ///
    /// With a lockfile these are the reproducible variants: they install
    /// exactly what is pinned and fail rather than updating it, so a preview
    /// builds the same tree twice. Without one they must not be — `npm ci`
    /// does not merely warn about a missing `package-lock.json`, it refuses
    /// to run, and a repository that does not commit its lockfile is common
    /// enough that generating a build which cannot start is the wrong
    /// default. Reproducibility is worth having; it is not worth refusing to
    /// deploy over.
    #[must_use]
    fn install(self, locked: bool) -> &'static str {
        match (self, locked) {
            (Self::Npm, true) => "npm ci",
            (Self::Npm, false) => "npm install",
            (Self::Pnpm, true) => "corepack enable && pnpm install --frozen-lockfile",
            (Self::Pnpm, false) => "corepack enable && pnpm install",
            (Self::Yarn, true) => "corepack enable && yarn install --immutable",
            (Self::Yarn, false) => "corepack enable && yarn install",
            (Self::Bun, true) => "bun install --frozen-lockfile",
            (Self::Bun, false) => "bun install",
        }
    }

    /// Installing only what production needs, for the stage that ships.
    #[must_use]
    fn prod_install(self, locked: bool) -> &'static str {
        match (self, locked) {
            (Self::Npm, true) => "npm ci --omit=dev",
            (Self::Npm, false) => "npm install --omit=dev",
            (Self::Pnpm, true) => "corepack enable && pnpm install --frozen-lockfile --prod",
            (Self::Pnpm, false) => "corepack enable && pnpm install --prod",
            // `workspaces focus` needs a resolved lockfile to focus against.
            (Self::Yarn, true) => "corepack enable && yarn workspaces focus --production",
            (Self::Yarn, false) => "corepack enable && yarn install --production",
            (Self::Bun, true) => "bun install --frozen-lockfile --production",
            (Self::Bun, false) => "bun install --production",
        }
    }

    #[must_use]
    fn run(self, script: &str) -> String {
        match self {
            Self::Npm => format!("npm run {script}"),
            Self::Pnpm => format!("pnpm run {script}"),
            Self::Yarn => format!("yarn {script}"),
            Self::Bun => format!("bun run {script}"),
        }
    }

    #[must_use]
    fn lockfile(self) -> &'static str {
        match self {
            Self::Npm => "package-lock.json",
            Self::Pnpm => "pnpm-lock.yaml",
            Self::Yarn => "yarn.lock",
            Self::Bun => "bun.lockb",
        }
    }
}

/// The framework on top of the runtime, when one is recognisable.
///
/// This is what decides the shape of the build: a Next.js app and an Express
/// server are both Node and need entirely different Dockerfiles.
///
/// Every variant is renamed explicitly rather than through a blanket
/// `rename_all`, because a derived kebab-case turns `NestJs` into
/// `nest-js` — a second spelling for the same thing, differing from
/// [`Framework::as_str`]. The wire name and the display name being the same
/// string is what lets the dashboard render a tag from the JSON without a
/// lookup table that would then have to be kept in step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Framework {
    #[serde(rename = "nestjs")]
    NestJs,
    #[serde(rename = "nextjs")]
    NextJs,
    /// Vite, Create React App, Angular — anything that builds to static
    /// files and needs a server only to hand them out.
    #[serde(rename = "spa")]
    SinglePageApp,
    /// Express, Fastify, Koa, or a plain `node server.js`.
    #[serde(rename = "node-server")]
    NodeServer,
    #[serde(rename = "fiber")]
    Fiber,
    #[serde(rename = "gin")]
    Gin,
    #[serde(rename = "echo")]
    Echo,
    /// A Go binary with no framework Oxid recognises.
    #[serde(rename = "go-server")]
    GoServer,
    #[serde(rename = "fastapi")]
    FastApi,
    #[serde(rename = "flask")]
    Flask,
    #[serde(rename = "django")]
    Django,
    #[serde(rename = "axum")]
    Axum,
    #[serde(rename = "actix")]
    Actix,
    /// A Rust binary with no framework Oxid recognises.
    #[serde(rename = "rust-server")]
    RustServer,
    #[serde(rename = "none")]
    None,
}

impl Framework {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NestJs => "nestjs",
            Self::NextJs => "nextjs",
            Self::SinglePageApp => "spa",
            Self::NodeServer => "node-server",
            Self::Fiber => "fiber",
            Self::Gin => "gin",
            Self::Echo => "echo",
            Self::GoServer => "go-server",
            Self::FastApi => "fastapi",
            Self::Flask => "flask",
            Self::Django => "django",
            Self::Axum => "axum",
            Self::Actix => "actix",
            Self::RustServer => "rust-server",
            Self::None => "none",
        }
    }

    /// What this framework listens on when nobody says otherwise. These are
    /// the framework's own documented defaults, not Oxid's preference.
    #[must_use]
    fn default_port(self) -> u16 {
        match self {
            Self::NestJs | Self::NextJs | Self::NodeServer => 3000,
            Self::SinglePageApp => 80,
            Self::FastApi | Self::Flask | Self::Django => 8000,
            _ => 8080,
        }
    }
}

/// How much of the result was read versus inferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// A manifest named the framework outright — it is a dependency, not a
    /// resemblance.
    Certain,
    /// The runtime is certain and the shape is a reasonable reading, but
    /// something was assumed. Worth showing the operator.
    Likely,
}

/// What a repository is, and what it takes to run it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stack {
    pub runtime: Runtime,
    pub framework: Framework,
    /// The runtime version the repository asks for, verbatim — `22`,
    /// `1.23.4`, `3.12`. `None` means it did not say, and a current default
    /// is used.
    pub runtime_version: Option<String>,
    pub package_manager: Option<PackageManager>,
    /// Whether a lockfile was actually found. Separate from
    /// `package_manager`, which falls back to npm so there is always
    /// *something* to run: without this the generated build would use the
    /// frozen install commands, and `npm ci` refuses to run at all when
    /// there is no `package-lock.json`.
    #[serde(default)]
    pub locked: bool,
    pub port: u16,
    pub confidence: Confidence,
    /// The specific files this conclusion was drawn from, so a wrong guess
    /// can be argued with rather than just disbelieved.
    pub evidence: Vec<String>,
    /// For compiled runtimes, the package path to build.
    ///
    /// `go build -o app ./...` looks tidy and breaks on any module with
    /// more than one `main` — "cannot write multiple packages to a single
    /// output". Where the entry point actually is, is visible from the
    /// repository, so it is read rather than assumed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_target: Option<String>,
}

impl Stack {
    /// A short label for a dashboard tag: `nestjs · node 22`.
    #[must_use]
    pub fn label(&self) -> String {
        let mut label = match self.framework {
            Framework::None => self.runtime.as_str().to_owned(),
            other => other.as_str().to_owned(),
        };
        if self.framework != Framework::None && self.runtime != Runtime::Static {
            label.push_str(" · ");
            label.push_str(self.runtime.as_str());
        }
        if let Some(version) = &self.runtime_version {
            label.push(' ');
            label.push_str(version);
        }
        label
    }
}

/// How a repository declares that it holds more than one package.
///
/// Which one is in use changes the install command, not just the label:
/// pnpm needs `--filter`, npm and yarn need `--workspace`, and installing a
/// workspace member as if it were a standalone project misses every
/// dependency it has on a sibling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceKind {
    /// `pnpm-workspace.yaml`.
    Pnpm,
    /// A `workspaces` array in the root `package.json` — npm, yarn and bun
    /// all read the same field.
    PackageJson,
    /// `lerna.json`, which predates the others and still exists.
    Lerna,
}

/// One deployable package inside a monorepo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    /// Path from the repository root, e.g. `apps/api`.
    pub path: String,
    /// The package's own name, which is what `--filter` takes.
    pub name: String,
    /// What that package is, detected the same way a standalone repository
    /// would be.
    pub framework: Framework,
    pub port: u16,
}

/// A repository holding several packages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Monorepo {
    pub kind: WorkspaceKind,
    /// Present when the repository also uses a task runner on top. It does
    /// not change the build, but it is worth reporting: it tells an
    /// operator the repository has a build graph, and that a member may
    /// depend on siblings being built first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_runner: Option<String>,
    /// Members that look deployable — something that serves traffic, as
    /// opposed to a shared library nobody runs on its own.
    pub deployable: Vec<Workspace>,
}

/// Detects a JavaScript monorepo and the deployable packages in it.
///
/// Returns `None` for an ordinary single-package repository, which is what
/// most repositories are.
#[must_use]
pub fn detect_monorepo(manifest: &RepoManifest) -> Option<Monorepo> {
    let root_package = manifest.read("package.json");

    let kind = if manifest.has("pnpm-workspace.yaml") {
        WorkspaceKind::Pnpm
    } else if root_package.is_some_and(|p| p.contains("\"workspaces\"")) {
        WorkspaceKind::PackageJson
    } else if manifest.has("lerna.json") {
        WorkspaceKind::Lerna
    } else {
        return None;
    };

    // Reported, not acted on. Turborepo and Nx orchestrate builds; the
    // container still installs and builds with the package manager
    // underneath, so this changes what an operator is told rather than what
    // is run.
    let task_runner = if manifest.has("turbo.json") {
        Some("turborepo".to_owned())
    } else if manifest.has("nx.json") {
        Some("nx".to_owned())
    } else {
        None
    };

    // Every path with its own `package.json` is a member. The workspace
    // globs are not parsed: a glob language (`packages/**`, negations,
    // `!packages/private-*`) is a lot of machinery to arrive at the same
    // answer that "has a package.json" gives directly, and the adapter has
    // already bounded where it looked.
    let mut deployable = Vec::new();
    for entry in &manifest.entries {
        let Some(dir) = entry.strip_suffix("/package.json") else {
            continue;
        };
        if dir.is_empty() || !entry.contains('/') {
            continue;
        }
        let Some(package) = manifest.member_package(dir) else {
            continue;
        };
        let Some(stack) = detect_node(&RepoManifest {
            entries: vec!["package.json".to_owned()],
            files: [("package.json".to_owned(), package.to_owned())]
                .into_iter()
                .collect(),
        }) else {
            continue;
        };
        if !is_deployable(package, stack.framework) {
            continue;
        }
        deployable.push(Workspace {
            path: dir.to_owned(),
            name: package_name(package).unwrap_or_else(|| dir.to_owned()),
            framework: stack.framework,
            port: stack.port,
        });
    }
    deployable.sort_by(|a, b| a.path.cmp(&b.path));

    Some(Monorepo {
        kind,
        task_runner,
        deployable,
    })
}

/// Whether a workspace member is something to deploy or something other
/// packages import.
///
/// The distinction matters because a monorepo is mostly libraries: listing
/// `packages/shared` as a deployable environment sends an operator to
/// register something that has nothing to serve.
///
/// Three things make a package deployable, and a start script alone is not
/// enough — a Fastify service whose entry point is `node src/index.js` in a
/// Compose file, with no `start` script, is still a service. What is
/// conclusive is depending on something that listens.
/// Packages whose presence means the member listens on a port, and so
/// is an environment rather than a library other packages import.
const SERVERS: &[&str] = &[
    "express",
    "fastify",
    "koa",
    "@hapi/hapi",
    "restify",
    "hono",
    "h3",
    "@nestjs/platform-express",
    "apollo-server",
    "@apollo/server",
    "graphql-yoga",
    "socket.io",
];

fn is_deployable(package_json: &str, framework: Framework) -> bool {
    // A recognised framework is a server by definition.
    if !matches!(framework, Framework::NodeServer | Framework::None) {
        return true;
    }
    // Something that listens on a port.
    if SERVERS
        .iter()
        .any(|s| package_json.contains(&format!("\"{s}\"")))
    {
        return true;
    }
    // Or something whose author says how to start it.
    package_json.contains("\"start\"")
}

/// The `name` field of a `package.json`, which is what a workspace filter
/// takes — the directory name and the package name differ often enough
/// (`apps/api` holding `@acme/billing-api`) that guessing is wrong.
fn package_name(package_json: &str) -> Option<String> {
    let rest = package_json.split("\"name\"").nth(1)?;
    let value = rest.split(':').nth(1)?;
    let name = value.split('"').nth(1)?;
    (!name.is_empty()).then(|| name.to_owned())
}

/// Reads `manifest` and decides what it is.
///
/// Returns `None` when nothing is recognisable, which is a real answer: a
/// repository Oxid cannot identify gets the same "write a Dockerfile" error
/// it always did, rather than a generated build that fails halfway through
/// for reasons the operator has to reverse-engineer.
#[must_use]
pub fn detect(manifest: &RepoManifest) -> Option<Stack> {
    detect_node(manifest)
        .or_else(|| detect_go(manifest))
        .or_else(|| detect_python(manifest))
        .or_else(|| detect_rust(manifest))
        .or_else(|| detect_static(manifest))
}

// ---------------------------------------------------------------------------
// Node
// ---------------------------------------------------------------------------

fn detect_node(manifest: &RepoManifest) -> Option<Stack> {
    let package_json = manifest.read("package.json")?;
    let mut evidence = vec!["package.json".to_owned()];

    let package_manager = detect_package_manager(manifest);
    if let Some(pm) = package_manager {
        evidence.push(pm.lockfile().to_owned());
    }

    // Dependency names are matched as JSON keys — `"next":` — rather than as
    // substrings. `"next"` appears inside `"next-auth"` and a dozen other
    // package names, and a project that merely uses `next-auth` is not a
    // Next.js app.
    let has_dep = |name: &str| package_json.contains(&format!("\"{name}\""));

    let framework = if has_dep("@nestjs/core") {
        evidence.push("@nestjs/core".to_owned());
        Framework::NestJs
    } else if has_dep("next") {
        evidence.push("next".to_owned());
        Framework::NextJs
    } else if has_dep("vite") || has_dep("react-scripts") || has_dep("@angular/core") {
        evidence.push(
            if has_dep("vite") {
                "vite"
            } else if has_dep("react-scripts") {
                "react-scripts"
            } else {
                "@angular/core"
            }
            .to_owned(),
        );
        Framework::SinglePageApp
    } else {
        Framework::NodeServer
    };

    // A version the repository states outright is certain; anything else is
    // a reading.
    let runtime_version = node_version(manifest);
    let confidence = if framework == Framework::NodeServer {
        Confidence::Likely
    } else {
        Confidence::Certain
    };

    Some(Stack {
        runtime: Runtime::Node,
        framework,
        runtime_version,
        package_manager: package_manager.or(Some(PackageManager::Npm)),
        locked: package_manager.is_some(),
        build_target: None,
        port: framework.default_port(),
        confidence,
        evidence,
    })
}

/// The lockfile decides, and the most specific one wins: a repository that
/// has both `pnpm-lock.yaml` and a stale `package-lock.json` is a pnpm
/// project whose npm lockfile nobody deleted.
fn detect_package_manager(manifest: &RepoManifest) -> Option<PackageManager> {
    for (file, pm) in [
        ("bun.lockb", PackageManager::Bun),
        ("pnpm-lock.yaml", PackageManager::Pnpm),
        ("yarn.lock", PackageManager::Yarn),
        ("package-lock.json", PackageManager::Npm),
    ] {
        if manifest.has(file) {
            return Some(pm);
        }
    }
    None
}

/// `.nvmrc` first: it is the file a developer's own shell obeys, so it is
/// the version they are actually running. `engines.node` is a range
/// (`>=20 <23`) more often than a version, and the lower bound of a range is
/// the safe reading of it.
fn node_version(manifest: &RepoManifest) -> Option<String> {
    if let Some(nvmrc) = manifest.read(".nvmrc") {
        let v = nvmrc.trim().trim_start_matches('v');
        if !v.is_empty() {
            return Some(v.split('.').next().unwrap_or(v).to_owned());
        }
    }
    let package_json = manifest.read("package.json")?;
    let engines = package_json.split("\"node\"").nth(1)?;
    let raw = engines.split('"').nth(1)?;
    let major: String = raw
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect();
    (!major.is_empty()).then_some(major)
}

// ---------------------------------------------------------------------------
// Go
// ---------------------------------------------------------------------------

fn detect_go(manifest: &RepoManifest) -> Option<Stack> {
    let go_mod = manifest.read("go.mod")?;
    let mut evidence = vec!["go.mod".to_owned()];

    let framework = if go_mod.contains("github.com/gofiber/fiber") {
        evidence.push("gofiber/fiber".to_owned());
        Framework::Fiber
    } else if go_mod.contains("github.com/gin-gonic/gin") {
        evidence.push("gin-gonic/gin".to_owned());
        Framework::Gin
    } else if go_mod.contains("github.com/labstack/echo") {
        evidence.push("labstack/echo".to_owned());
        Framework::Echo
    } else {
        Framework::GoServer
    };

    // `go 1.23` in go.mod is the language version the module targets, which
    // is exactly what the build image has to provide.
    let runtime_version = go_mod.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("go ")?;
        let v = rest.trim();
        v.chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
            .then(|| v.to_owned())
    });

    // `main.go` beside `go.mod` is the overwhelmingly common shape of a
    // single service; `cmd/` is the other convention, and there the module
    // may hold several binaries, so the whole tree is left to `go build`
    // and its own error explains what to pick.
    let build_target = if manifest.has("main.go") {
        Some(".".to_owned())
    } else {
        None
    };

    Some(Stack {
        runtime: Runtime::Go,
        framework,
        runtime_version,
        package_manager: None,
        locked: manifest.has("go.sum"),
        build_target,
        port: framework.default_port(),
        confidence: if framework == Framework::GoServer {
            Confidence::Likely
        } else {
            Confidence::Certain
        },
        evidence,
    })
}

// ---------------------------------------------------------------------------
// Python
// ---------------------------------------------------------------------------

fn detect_python(manifest: &RepoManifest) -> Option<Stack> {
    let deps = manifest
        .read("pyproject.toml")
        .or_else(|| manifest.read("requirements.txt"))?;
    let source = if manifest.read("pyproject.toml").is_some() {
        "pyproject.toml"
    } else {
        "requirements.txt"
    };
    let mut evidence = vec![source.to_owned()];
    let lower = deps.to_ascii_lowercase();

    let framework = if lower.contains("fastapi") {
        evidence.push("fastapi".to_owned());
        Framework::FastApi
    } else if lower.contains("django") {
        evidence.push("django".to_owned());
        Framework::Django
    } else if lower.contains("flask") {
        evidence.push("flask".to_owned());
        Framework::Flask
    } else {
        Framework::None
    };

    // Nothing to run without a web framework: a library or a script is not
    // an environment, and generating a container that exits immediately
    // helps nobody.
    if framework == Framework::None {
        return None;
    }

    Some(Stack {
        runtime: Runtime::Python,
        framework,
        runtime_version: python_version(&lower),
        package_manager: None,
        locked: false,
        build_target: None,
        port: framework.default_port(),
        confidence: Confidence::Certain,
        evidence,
    })
}

fn python_version(pyproject: &str) -> Option<String> {
    let rest = pyproject.split("requires-python").nth(1)?;
    let spec = rest.split('"').nth(1)?;
    let digits: String = spec
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    (!digits.is_empty()).then_some(digits)
}

// ---------------------------------------------------------------------------
// Rust
// ---------------------------------------------------------------------------

fn detect_rust(manifest: &RepoManifest) -> Option<Stack> {
    let cargo = manifest.read("Cargo.toml")?;
    let mut evidence = vec!["Cargo.toml".to_owned()];

    let framework = if cargo.contains("axum") {
        evidence.push("axum".to_owned());
        Framework::Axum
    } else if cargo.contains("actix-web") {
        evidence.push("actix-web".to_owned());
        Framework::Actix
    } else {
        Framework::RustServer
    };

    // Cargo names the binary after the package, so the final stage has to
    // copy that exact path. Assuming `app` meant the generated build failed
    // for every crate not called `app` — which is all of them.
    let build_target = cargo_package_name(cargo);

    Some(Stack {
        runtime: Runtime::Rust,
        framework,
        runtime_version: None,
        package_manager: None,
        locked: manifest.has("Cargo.lock"),
        build_target,
        port: framework.default_port(),
        confidence: if framework == Framework::RustServer {
            Confidence::Likely
        } else {
            Confidence::Certain
        },
        evidence,
    })
}

/// The `[package] name` from a Cargo manifest, which is what the compiled
/// binary is called.
///
/// Read rather than assumed: the final stage copies one exact path, and
/// getting it wrong fails the build after the slowest step in it.
fn cargo_package_name(cargo: &str) -> Option<String> {
    let mut in_package = false;
    for line in cargo.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            // `[package]` only — `name` also appears under `[[bin]]`,
            // `[lib]` and dependency tables.
            in_package = line == "[package]";
            continue;
        }
        if in_package && let Some(rest) = line.strip_prefix("name") {
            let value = rest.trim_start().strip_prefix('=')?.trim();
            let name = value.trim_matches(['"', '\''].as_slice());
            if !name.is_empty() {
                return Some(name.to_owned());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Static
// ---------------------------------------------------------------------------

fn detect_static(manifest: &RepoManifest) -> Option<Stack> {
    manifest.has("index.html").then(|| Stack {
        runtime: Runtime::Static,
        framework: Framework::None,
        runtime_version: None,
        package_manager: None,
        locked: false,
        build_target: None,
        port: 80,
        confidence: Confidence::Likely,
        evidence: vec!["index.html".to_owned()],
    })
}

// ---------------------------------------------------------------------------
// Containerising
// ---------------------------------------------------------------------------

impl Monorepo {
    /// A Dockerfile that builds one member of the workspace.
    ///
    /// The reason this cannot be the ordinary single-package Dockerfile:
    /// a workspace member's dependencies are not all in its own
    /// `package.json`. It imports siblings (`@acme/shared`), and the
    /// lockfile that resolves them lives at the repository root. Installing
    /// from inside `apps/api` finds neither — the build fails on an import
    /// the developer can see working locally, which is the worst kind of
    /// failure to hand someone.
    ///
    /// So the build context is the whole repository and the install runs at
    /// the root, filtered to the target and the siblings it needs. Only the
    /// manifests are copied before installing, so the layer survives every
    /// commit that does not change a dependency — which in a monorepo is
    /// most of them, and where the time goes.
    #[must_use]
    pub fn dockerfile(
        &self,
        member: &Workspace,
        node_version: Option<&str>,
        pm: PackageManager,
        locked: bool,
    ) -> String {
        let base = format!("node:{}-alpine", node_version.unwrap_or("22"));
        let path = &member.path;
        let name = &member.name;
        let runner = self
            .task_runner
            .as_deref()
            .map_or(String::new(), |r| format!(", orchestrated by {r}"));

        // Each manager scopes an install differently, and getting it wrong
        // either installs the entire monorepo or none of the siblings.
        let (install, build) = match self.kind {
            WorkspaceKind::Pnpm => (
                format!(
                    "corepack enable && pnpm install {} --filter {name}...",
                    if locked { "--frozen-lockfile" } else { "" }
                ),
                format!("corepack enable && pnpm --filter {name} run build"),
            ),
            _ => match pm {
                PackageManager::Yarn => (
                    format!(
                        "corepack enable && yarn install{}",
                        if locked { " --immutable" } else { "" }
                    ),
                    format!("corepack enable && yarn workspace {name} run build"),
                ),
                PackageManager::Bun => (
                    "bun install".to_owned(),
                    format!("bun run --filter {name} build"),
                ),
                _ => (
                    format!("npm {}", if locked { "ci" } else { "install" }),
                    format!("npm run build --workspace {name}"),
                ),
            },
        };

        // Nest and Next put their output in different places, and a SPA has
        // no server at all — the same three shapes as a standalone project,
        // resolved against the member's directory.
        let (final_stage, cmd) = match member.framework {
            Framework::SinglePageApp => (
                format!(
                    "FROM nginx:alpine\n\
                     COPY --from=build /repo/{path}/dist* /repo/{path}/build* /usr/share/nginx/html/\n"
                ),
                String::new(),
            ),
            Framework::NextJs => (
                format!(
                    "FROM {base}\n\
                     WORKDIR /app\n\
                     ENV NODE_ENV=production\n\
                     # Needs `output: \"standalone\"` in next.config.js, which in a\n\
                     # workspace also emits the siblings it traced.\n\
                     COPY --from=build /repo/{path}/.next/standalone ./\n\
                     COPY --from=build /repo/{path}/.next/static ./{path}/.next/static\n"
                ),
                format!("CMD [\"node\", \"{path}/server.js\"]\n"),
            ),
            _ => (
                format!(
                    "FROM {base}\n\
                     WORKDIR /app\n\
                     ENV NODE_ENV=production\n\
                     # The whole built tree, deliberately.\n\
                     #\n\
                     # Copying `node_modules` plus this one package looks\n\
                     # tighter and does not work: a workspace links siblings\n\
                     # into `node_modules` as symlinks pointing back at\n\
                     # `packages/*`, so leaving those behind produces an image\n\
                     # that starts and dies on MODULE_NOT_FOUND for an import\n\
                     # the developer can see working locally. Which packages a\n\
                     # member links to is a question only the resolver can\n\
                     # answer, so the answer is to carry the tree.\n\
                     COPY --from=build /repo ./\n"
                ),
                format!("CMD [\"node\", \"{path}/dist/main.js\"]\n"),
            ),
        };

        format!(
            "# Generated by Oxid for `{name}` ({path}) in a {kind} workspace{runner}.\n\
             #\n\
             # The context is the repository root, not this package: its\n\
             # dependencies include siblings, and the lockfile that resolves\n\
             # them is at the root. Commit a `Dockerfile` and Oxid uses it.\n\
             \n\
             FROM {base} AS build\n\
             WORKDIR /repo\n\
             # Manifests first, so the install layer survives every commit\n\
             # that does not change a dependency.\n\
             COPY package.json *lock* pnpm-workspace.yaml* ./\n\
             COPY {path}/package.json {path}/\n\
             RUN {install}\n\
             COPY . .\n\
             RUN {build}\n\
             \n\
             {final_stage}\
             EXPOSE {port}\n\
             {cmd}",
            kind = match self.kind {
                WorkspaceKind::Pnpm => "pnpm",
                WorkspaceKind::PackageJson => "npm/yarn/bun",
                WorkspaceKind::Lerna => "lerna",
            },
            port = member.port,
        )
    }
}

impl Stack {
    /// A Dockerfile for this stack.
    ///
    /// Every one of these is multi-stage, and that is the whole point: the
    /// image that ships carries the built artifact and its runtime
    /// dependencies, not the toolchain, the caches and the source. A
    /// hand-written first Dockerfile is usually single-stage, which on a
    /// node with a dozen preview environments is the difference between
    /// gigabytes and hundreds of megabytes — and this product's entire
    /// premise is fitting many environments on one host.
    ///
    /// Dependencies are installed before the source is copied, so a commit
    /// that does not touch the lockfile reuses the install layer. On a
    /// preview environment redeployed on every push, that layer is most of
    /// the build time.
    #[must_use]
    pub fn dockerfile(&self) -> String {
        match self.runtime {
            Runtime::Node => self.node_dockerfile(),
            Runtime::Go => self.go_dockerfile(),
            Runtime::Python => self.python_dockerfile(),
            Runtime::Rust => self.rust_dockerfile(),
            Runtime::Static => Self::static_dockerfile(),
        }
    }

    /// The banner every generated Dockerfile opens with. Whoever finds this
    /// file in a build log needs to know it was not written by a colleague.
    fn header(&self) -> String {
        format!(
            "# Generated by Oxid from {evidence}.\n\
             #\n\
             # Detected: {label} (confidence: {confidence}).\n\
             # This is a starting point, not a decision. Commit a `Dockerfile`\n\
             # of your own and Oxid will use it instead — it never overwrites one.\n",
            evidence = self.evidence.join(", "),
            label = self.label(),
            confidence = match self.confidence {
                Confidence::Certain => "certain",
                Confidence::Likely => "likely",
            },
        )
    }

    fn node_base(&self) -> String {
        // Alpine: the runtime stage is a tenth the size of the Debian one,
        // and nothing in these builds needs glibc.
        format!(
            "node:{}-alpine",
            self.runtime_version.as_deref().unwrap_or("22")
        )
    }

    fn node_dockerfile(&self) -> String {
        let pm = self.package_manager.unwrap_or(PackageManager::Npm);
        let base = self.node_base();
        let header = self.header();
        let port = self.port;

        match self.framework {
            // A SPA has no server of its own: it builds to a directory of
            // files, and what ships is a static server holding them. No
            // Node in the final image at all.
            Framework::SinglePageApp => format!(
                "{header}\n\
                 FROM {base} AS build\n\
                 WORKDIR /app\n\
                 COPY package.json {lock}* ./\n\
                 RUN {install}\n\
                 COPY . .\n\
                 RUN {build}\n\
                 \n\
                 FROM nginx:alpine\n\
                 # Vite and Angular emit `dist`, Create React App emits\n\
                 # `build`. Copying whichever exists avoids asking.\n\
                 COPY --from=build /app/dist* /app/build* /usr/share/nginx/html/\n\
                 EXPOSE {port}\n",
                lock = pm.lockfile(),
                install = pm.install(self.locked),
                build = pm.run("build"),
            ),
            // Next.js in standalone output mode ships a self-contained
            // server directory; without it the runtime stage would need the
            // whole `node_modules` back.
            Framework::NextJs => format!(
                "{header}\n\
                 FROM {base} AS build\n\
                 WORKDIR /app\n\
                 COPY package.json {lock}* ./\n\
                 RUN {install}\n\
                 COPY . .\n\
                 RUN {build}\n\
                 \n\
                 FROM {base}\n\
                 WORKDIR /app\n\
                 ENV NODE_ENV=production\n\
                 # Needs `output: \"standalone\"` in next.config.js. Without it,\n\
                 # swap these three lines for a copy of `node_modules` and\n\
                 # `CMD [\"{pm_name}\", \"start\"]`.\n\
                 COPY --from=build /app/.next/standalone ./\n\
                 COPY --from=build /app/.next/static ./.next/static\n\
                 COPY --from=build /app/public ./public\n\
                 EXPOSE {port}\n\
                 CMD [\"node\", \"server.js\"]\n",
                lock = pm.lockfile(),
                install = pm.install(self.locked),
                build = pm.run("build"),
                pm_name = pm.as_str(),
            ),
            // Nest compiles to `dist`, and the runtime stage reinstalls
            // production dependencies rather than carrying the dev ones.
            Framework::NestJs => format!(
                "{header}\n\
                 FROM {base} AS build\n\
                 WORKDIR /app\n\
                 COPY package.json {lock}* ./\n\
                 RUN {install}\n\
                 COPY . .\n\
                 RUN {build}\n\
                 \n\
                 FROM {base}\n\
                 WORKDIR /app\n\
                 ENV NODE_ENV=production\n\
                 COPY package.json {lock}* ./\n\
                 RUN {prod_install}\n\
                 COPY --from=build /app/dist ./dist\n\
                 EXPOSE {port}\n\
                 CMD [\"node\", \"dist/main.js\"]\n",
                lock = pm.lockfile(),
                install = pm.install(self.locked),
                build = pm.run("build"),
                prod_install = pm.prod_install(self.locked),
            ),
            // A plain server: it may or may not have a build step, so the
            // start script is what runs and the build is left to the
            // developer to add if they need one.
            _ => format!(
                "{header}\n\
                 FROM {base}\n\
                 WORKDIR /app\n\
                 ENV NODE_ENV=production\n\
                 COPY package.json {lock}* ./\n\
                 RUN {prod_install}\n\
                 COPY . .\n\
                 EXPOSE {port}\n\
                 CMD [\"{pm_name}\", \"start\"]\n",
                lock = pm.lockfile(),
                prod_install = pm.prod_install(self.locked),
                pm_name = pm.as_str(),
            ),
        }
    }

    fn go_dockerfile(&self) -> String {
        // A statically linked binary needs nothing around it, so the final
        // image is the binary and a certificate bundle. Tens of megabytes
        // against the ~800MB of a `golang` image kept as the runtime.
        format!(
            "{header}\n\
             FROM golang:{version}-alpine AS build\n\
             WORKDIR /src\n\
             COPY go.mod go.sum* ./\n\
             RUN go mod download\n\
             COPY . .\n\
             # CGO off: without it the binary links against musl and will not\n\
             # run in the scratch stage below.\n\
             RUN CGO_ENABLED=0 go build -ldflags=\"-s -w\" -o /out/app {target}\n\
             \n\
             FROM alpine:latest\n\
             RUN apk add --no-cache ca-certificates\n\
             COPY --from=build /out/app /app\n\
             EXPOSE {port}\n\
             CMD [\"/app\"]\n",
            header = self.header(),
            version = self.runtime_version.as_deref().unwrap_or("1.23"),
            target = self.build_target.as_deref().unwrap_or("./..."),
            port = self.port,
        )
    }

    fn python_dockerfile(&self) -> String {
        let version = self.runtime_version.as_deref().unwrap_or("3.12");
        let install = if self.evidence.iter().any(|e| e == "pyproject.toml") {
            "pip install --no-cache-dir ."
        } else {
            "pip install --no-cache-dir -r requirements.txt"
        };
        let copy = if self.evidence.iter().any(|e| e == "pyproject.toml") {
            "COPY pyproject.toml ./"
        } else {
            "COPY requirements.txt ./"
        };
        let cmd = match self.framework {
            // Uvicorn needs a module path it cannot infer; `main:app` is
            // the convention FastAPI's own tutorial uses.
            Framework::FastApi => format!(
                "CMD [\"uvicorn\", \"main:app\", \"--host\", \"0.0.0.0\", \"--port\", \"{}\"]",
                self.port
            ),
            Framework::Django => format!(
                "CMD [\"python\", \"manage.py\", \"runserver\", \"0.0.0.0:{}\"]",
                self.port
            ),
            _ => format!(
                "CMD [\"flask\", \"run\", \"--host=0.0.0.0\", \"--port={}\"]",
                self.port
            ),
        };
        format!(
            "{header}\n\
             FROM python:{version}-slim\n\
             WORKDIR /app\n\
             ENV PYTHONUNBUFFERED=1\n\
             {copy}\n\
             RUN {install}\n\
             COPY . .\n\
             EXPOSE {port}\n\
             {cmd}\n",
            header = self.header(),
            port = self.port,
        )
    }

    fn rust_dockerfile(&self) -> String {
        format!(
            "{header}\n\
             FROM rust:1-slim AS build\n\
             WORKDIR /src\n\
             COPY . .\n\
             RUN cargo build --release\n\
             \n\
             FROM debian:stable-slim\n\
             RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \\\n\
             \x20   && rm -rf /var/lib/apt/lists/*\n\
             COPY --from=build /src/target/release/{binary} /app\n\
             EXPOSE {port}\n\
             CMD [\"/app\"]\n",
            header = self.header(),
            binary = self.build_target.as_deref().unwrap_or("app"),
            port = self.port,
        )
    }

    fn static_dockerfile() -> String {
        "# Generated by Oxid: a directory of files, served as files.\n\
         #\n\
         # Commit a `Dockerfile` of your own and Oxid will use it instead.\n\
         \n\
         FROM nginx:alpine\n\
         COPY . /usr/share/nginx/html/\n\
         EXPOSE 80\n"
            .to_owned()
    }
}

#[cfg(test)]
mod tests;
