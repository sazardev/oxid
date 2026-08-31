use super::*;

/// Builds a manifest from `(path, contents)` pairs. Anything with contents
/// is also "present", which is how a real repository works.
fn repo(files: &[(&str, &str)]) -> RepoManifest {
    let mut manifest = RepoManifest::default();
    for (path, body) in files {
        manifest.entries.push((*path).to_owned());
        if !body.is_empty() {
            manifest
                .files
                .insert((*path).to_owned(), (*body).to_owned());
        }
    }
    manifest
}

const NEST_PACKAGE: &str = r#"{
  "name": "billing-api",
  "scripts": { "build": "nest build", "start:prod": "node dist/main" },
  "dependencies": { "@nestjs/core": "^10.0.0", "@nestjs/common": "^10.0.0" }
}"#;

const VITE_PACKAGE: &str = r#"{
  "name": "storefront",
  "scripts": { "build": "vite build" },
  "devDependencies": { "vite": "^5.0.0", "react": "^18.2.0" }
}"#;

// ---------------------------------------------------------------------------
// detection
// ---------------------------------------------------------------------------

#[test]
fn a_nest_service_is_recognised_from_its_dependency() {
    let stack = detect(&repo(&[
        ("package.json", NEST_PACKAGE),
        ("package-lock.json", "{}"),
    ]))
    .unwrap();

    assert_eq!(stack.runtime, Runtime::Node);
    assert_eq!(stack.framework, Framework::NestJs);
    assert_eq!(stack.package_manager, Some(PackageManager::Npm));
    // Nest's own default, not Oxid's preference.
    assert_eq!(stack.port, 3000);
    assert_eq!(stack.confidence, Confidence::Certain);
    assert!(stack.evidence.contains(&"@nestjs/core".to_owned()));
}

#[test]
fn a_vite_app_is_a_single_page_app_whatever_the_ui_library() {
    for library in ["react", "vue", "svelte"] {
        let package = VITE_PACKAGE.replace("react", library);
        let stack = detect(&repo(&[("package.json", &package)])).unwrap();
        assert_eq!(stack.framework, Framework::SinglePageApp, "{library}");
        // Nothing to run: it ships as files behind a static server on 80.
        assert_eq!(stack.port, 80, "{library}");
    }
}

#[test]
fn a_dependency_is_matched_as_a_name_not_a_substring() {
    // `next-auth` contains "next", and a project using it for sessions is
    // not a Next.js app. Matching loosely here would generate a Next build
    // for an Express server and fail with an error about `.next/standalone`
    // that names nothing the developer wrote.
    let package = r#"{
      "dependencies": { "express": "^4.19.0", "next-auth": "^4.24.0" }
    }"#;
    let stack = detect(&repo(&[("package.json", package)])).unwrap();
    assert_eq!(stack.framework, Framework::NodeServer);
}

#[test]
fn the_lockfile_decides_the_package_manager_and_the_most_specific_wins() {
    for (lock, expected) in [
        ("pnpm-lock.yaml", PackageManager::Pnpm),
        ("yarn.lock", PackageManager::Yarn),
        ("bun.lockb", PackageManager::Bun),
        ("package-lock.json", PackageManager::Npm),
    ] {
        let stack = detect(&repo(&[("package.json", NEST_PACKAGE), (lock, "")])).unwrap();
        assert_eq!(stack.package_manager, Some(expected), "{lock}");
    }

    // A pnpm project with a stale package-lock.json left behind is still a
    // pnpm project — installing it with npm resolves a different tree than
    // the one the developer tested.
    let both = detect(&repo(&[
        ("package.json", NEST_PACKAGE),
        ("package-lock.json", ""),
        ("pnpm-lock.yaml", ""),
    ]))
    .unwrap();
    assert_eq!(both.package_manager, Some(PackageManager::Pnpm));
}

#[test]
fn the_node_version_comes_from_nvmrc_first_then_engines() {
    let with_nvmrc = detect(&repo(&[
        ("package.json", r#"{"engines":{"node":">=18"}}"#),
        (".nvmrc", "v22.11.0\n"),
    ]))
    .unwrap();
    // `.nvmrc` is what the developer's own shell obeys, so it beats a range
    // in `engines` that may not have been touched in two years.
    assert_eq!(with_nvmrc.runtime_version.as_deref(), Some("22"));

    let range = detect(&repo(&[(
        "package.json",
        r#"{"engines":{"node":">=20 <23"}}"#,
    )]))
    .unwrap();
    // The lower bound is the safe reading of a range.
    assert_eq!(range.runtime_version.as_deref(), Some("20"));

    let silent = detect(&repo(&[("package.json", "{}")])).unwrap();
    assert_eq!(silent.runtime_version, None);
}

#[test]
fn go_frameworks_are_recognised_and_the_language_version_is_read() {
    let go_mod =
        "module example.com/api\n\ngo 1.23\n\nrequire github.com/gofiber/fiber/v2 v2.52.0\n";
    let stack = detect(&repo(&[("go.mod", go_mod)])).unwrap();

    assert_eq!(stack.runtime, Runtime::Go);
    assert_eq!(stack.framework, Framework::Fiber);
    assert_eq!(stack.runtime_version.as_deref(), Some("1.23"));
    assert_eq!(stack.confidence, Confidence::Certain);

    for (module, expected) in [
        ("github.com/gin-gonic/gin", Framework::Gin),
        ("github.com/labstack/echo/v4", Framework::Echo),
    ] {
        let stack = detect(&repo(&[(
            "go.mod",
            &format!("module x\n\ngo 1.22\n\nrequire {module} v1.0.0\n"),
        )]))
        .unwrap();
        assert_eq!(stack.framework, expected, "{module}");
    }
}

#[test]
fn a_go_module_with_no_framework_is_still_go_but_only_likely() {
    let stack = detect(&repo(&[("go.mod", "module example.com/tool\n\ngo 1.22\n")])).unwrap();
    assert_eq!(stack.framework, Framework::GoServer);
    // The runtime is certain; that it serves HTTP on 8080 is not.
    assert_eq!(stack.confidence, Confidence::Likely);
}

#[test]
fn python_needs_a_web_framework_to_be_an_environment() {
    let api = detect(&repo(&[(
        "requirements.txt",
        "fastapi==0.115.0\nuvicorn\n",
    )]))
    .unwrap();
    assert_eq!(api.framework, Framework::FastApi);
    assert_eq!(api.port, 8000);

    // A library or a batch script is not something to deploy. Generating a
    // container that starts and immediately exits helps nobody.
    assert!(detect(&repo(&[("requirements.txt", "requests\nnumpy\n")])).is_none());
}

#[test]
fn a_directory_of_files_is_a_static_site() {
    let stack = detect(&repo(&[("index.html", "<h1>hi</h1>")])).unwrap();
    assert_eq!(stack.runtime, Runtime::Static);
    assert_eq!(stack.port, 80);
}

#[test]
fn a_repository_with_nothing_recognisable_is_not_guessed_at() {
    // The honest answer. An unrecognised repository gets the same "write a
    // Dockerfile" error it always did, rather than a generated build that
    // fails halfway through for reasons nobody can trace back.
    assert!(detect(&repo(&[("README.md", "# hello")])).is_none());
    assert!(detect(&RepoManifest::default()).is_none());
}

#[test]
fn node_is_preferred_over_a_language_that_only_tools_the_repository() {
    // A Node service whose CI is written in Go still deploys as Node.
    // Order matters here and this is what pins it.
    let stack = detect(&repo(&[
        ("package.json", NEST_PACKAGE),
        ("go.mod", "module tools\n\ngo 1.22\n"),
    ]))
    .unwrap();
    assert_eq!(stack.runtime, Runtime::Node);
}

#[test]
fn the_wire_name_and_the_display_name_are_the_same_string() {
    // The dashboard renders its tag from the JSON. If serde spelled a
    // variant differently from `as_str` — a derived kebab-case turns
    // `NestJs` into `nest-js` — the panel would show one name and the logs
    // another, for the same thing.
    for framework in [
        Framework::NestJs,
        Framework::NextJs,
        Framework::SinglePageApp,
        Framework::NodeServer,
        Framework::Fiber,
        Framework::Gin,
        Framework::Echo,
        Framework::GoServer,
        Framework::FastApi,
        Framework::Flask,
        Framework::Django,
        Framework::Axum,
        Framework::Actix,
        Framework::RustServer,
        Framework::None,
    ] {
        let json = serde_json::to_string(&framework).unwrap();
        assert_eq!(
            json.trim_matches('"'),
            framework.as_str(),
            "{framework:?} serializes differently from how it displays"
        );
    }
    for runtime in [
        Runtime::Node,
        Runtime::Go,
        Runtime::Python,
        Runtime::Rust,
        Runtime::Static,
    ] {
        let json = serde_json::to_string(&runtime).unwrap();
        assert_eq!(json.trim_matches('"'), runtime.as_str(), "{runtime:?}");
    }
}

#[test]
fn the_label_reads_as_a_tag() {
    let stack = detect(&repo(&[
        ("package.json", NEST_PACKAGE),
        (".nvmrc", "22"),
        ("pnpm-lock.yaml", ""),
    ]))
    .unwrap();
    assert_eq!(stack.label(), "nestjs · node 22");

    let go = detect(&repo(&[(
        "go.mod",
        "module x\n\ngo 1.23\n\nrequire github.com/gofiber/fiber/v2 v2.0.0\n",
    )]))
    .unwrap();
    assert_eq!(go.label(), "fiber · go 1.23");
}

// ---------------------------------------------------------------------------
// generated Dockerfiles
// ---------------------------------------------------------------------------

#[test]
fn every_detectable_stack_produces_a_dockerfile_that_says_where_it_came_from() {
    let cases = [
        repo(&[("package.json", NEST_PACKAGE), ("package-lock.json", "")]),
        repo(&[("package.json", VITE_PACKAGE)]),
        repo(&[("package.json", r#"{"dependencies":{"next":"14.0.0"}}"#)]),
        repo(&[("package.json", r#"{"dependencies":{"express":"4.19.0"}}"#)]),
        repo(&[("go.mod", "module x\n\ngo 1.23\n")]),
        repo(&[("requirements.txt", "fastapi\n")]),
        repo(&[("Cargo.toml", "[dependencies]\naxum = \"0.7\"\n")]),
        repo(&[("index.html", "<h1>hi</h1>")]),
    ];
    for manifest in cases {
        let stack = detect(&manifest).unwrap();
        let dockerfile = stack.dockerfile();
        assert!(
            dockerfile.starts_with("# Generated by Oxid"),
            "{}: whoever finds this in a build log must know it was not \
             written by a colleague\n{dockerfile}",
            stack.label()
        );
        assert!(
            dockerfile.contains("FROM "),
            "{}: not a Dockerfile\n{dockerfile}",
            stack.label()
        );
        assert!(
            dockerfile.contains(&format!("EXPOSE {}", stack.port)),
            "{}: exposes a different port than it detected\n{dockerfile}",
            stack.label()
        );
    }
}

#[test]
fn compiled_stacks_ship_the_artifact_and_not_the_toolchain() {
    // The premise of this product is fitting many environments on one host.
    // A single-stage image carries the compiler, the caches and the source
    // into every one of them.
    let go = detect(&repo(&[("go.mod", "module x\n\ngo 1.23\n")]))
        .unwrap()
        .dockerfile();
    assert_eq!(go.matches("FROM ").count(), 2, "{go}");
    assert!(
        go.contains("CGO_ENABLED=0"),
        "static linking is what makes the small final stage possible\n{go}"
    );
    assert!(
        !go.contains("FROM golang:1.23-alpine\nRUN"),
        "the toolchain must not be the runtime\n{go}"
    );

    let nest = detect(&repo(&[("package.json", NEST_PACKAGE)]))
        .unwrap()
        .dockerfile();
    assert_eq!(nest.matches("FROM ").count(), 2, "{nest}");
    assert!(
        nest.contains("--omit=dev"),
        "dev dependencies must not ship\n{nest}"
    );

    // A SPA needs no Node at all once it is built.
    let spa = detect(&repo(&[("package.json", VITE_PACKAGE)]))
        .unwrap()
        .dockerfile();
    assert!(spa.contains("FROM nginx:alpine"), "{spa}");
    assert!(
        !spa.split("FROM nginx:alpine")
            .nth(1)
            .unwrap()
            .contains("node"),
        "the runtime stage still carries Node\n{spa}"
    );
}

#[test]
fn dependencies_are_installed_before_the_source_is_copied() {
    // On a preview environment rebuilt on every push, the install layer is
    // most of the build. Copying the source first invalidates it every
    // time, which turns a ten-second redeploy into a two-minute one.
    for manifest in [
        repo(&[("package.json", NEST_PACKAGE), ("pnpm-lock.yaml", "")]),
        repo(&[("go.mod", "module x\n\ngo 1.23\n")]),
        repo(&[("requirements.txt", "fastapi\n")]),
    ] {
        let stack = detect(&manifest).unwrap();
        let dockerfile = stack.dockerfile();
        let install = dockerfile
            .find("RUN ")
            .unwrap_or_else(|| panic!("{}: no install step\n{dockerfile}", stack.label()));
        let copy_all = dockerfile
            .find("COPY . .")
            .unwrap_or_else(|| panic!("{}: never copies the source\n{dockerfile}", stack.label()));
        assert!(
            install < copy_all,
            "{}: the source is copied before dependencies are installed, so \
             every commit re-installs everything\n{dockerfile}",
            stack.label()
        );
    }
}

#[test]
fn the_package_manager_reaches_the_generated_build() {
    // Installing a pnpm project with `npm ci` fails outright: there is no
    // package-lock.json to install from.
    let pnpm = detect(&repo(&[
        ("package.json", NEST_PACKAGE),
        ("pnpm-lock.yaml", ""),
    ]))
    .unwrap()
    .dockerfile();
    assert!(pnpm.contains("pnpm install --frozen-lockfile"), "{pnpm}");
    assert!(
        pnpm.contains("pnpm-lock.yaml"),
        "the lockfile is never copied\n{pnpm}"
    );
    assert!(!pnpm.contains("npm ci"), "{pnpm}");

    let yarn = detect(&repo(&[("package.json", NEST_PACKAGE), ("yarn.lock", "")]))
        .unwrap()
        .dockerfile();
    assert!(yarn.contains("yarn install --immutable"), "{yarn}");
}

#[test]
fn a_project_without_a_lockfile_gets_an_install_that_can_actually_run() {
    // `npm ci` does not warn about a missing package-lock.json — it refuses
    // to run. Defaulting to it meant every repository that does not commit
    // its lockfile got a build that could not start, which is a large share
    // of them. Found by building a real Vite app, not by reading the code.
    let unlocked = detect(&repo(&[("package.json", VITE_PACKAGE)])).unwrap();
    assert!(!unlocked.locked);
    let dockerfile = unlocked.dockerfile();
    assert!(
        dockerfile.contains("npm install"),
        "still emits an install that cannot run without a lockfile\n{dockerfile}"
    );
    assert!(!dockerfile.contains("npm ci"), "{dockerfile}");

    // With a lockfile the reproducible variant is right, and is what
    // guarantees two builds of the same commit install the same tree.
    let locked = detect(&repo(&[
        ("package.json", VITE_PACKAGE),
        ("package-lock.json", "{}"),
    ]))
    .unwrap();
    assert!(locked.locked);
    assert!(locked.dockerfile().contains("npm ci"));

    for (lock, frozen) in [
        ("pnpm-lock.yaml", "--frozen-lockfile"),
        ("yarn.lock", "--immutable"),
    ] {
        let with = detect(&repo(&[("package.json", VITE_PACKAGE), (lock, "")]))
            .unwrap()
            .dockerfile();
        assert!(with.contains(frozen), "{lock}: {with}");
    }
}

#[test]
fn the_go_build_target_comes_from_where_main_actually_is() {
    // `go build -o app ./...` breaks on any module with more than one
    // `main` package: "cannot write multiple packages to a single output".
    // Where the entry point is, is visible from the repository.
    let at_root = detect(&repo(&[
        ("go.mod", "module x\n\ngo 1.23\n"),
        ("main.go", "package main"),
    ]))
    .unwrap();
    assert_eq!(at_root.build_target.as_deref(), Some("."));
    assert!(
        at_root.dockerfile().contains("-o /out/app .\n"),
        "{}",
        at_root.dockerfile()
    );

    // No `main.go` at the root: the module may hold several binaries under
    // `cmd/`, so the whole tree is left to `go build`, whose own error says
    // which to pick.
    let elsewhere = detect(&repo(&[("go.mod", "module x\n\ngo 1.23\n")])).unwrap();
    assert_eq!(elsewhere.build_target, None);
    assert!(elsewhere.dockerfile().contains("./..."));
}

#[test]
fn the_rust_binary_is_named_from_the_manifest_not_assumed() {
    // Cargo names the binary after the package. Assuming `app` meant the
    // generated build failed for every crate not called `app` — which is
    // all of them — and it failed after the slowest step in the build.
    let cargo = "[package]\nname = \"billing-worker\"\nversion = \"0.1.0\"\n\n[dependencies]\naxum = \"0.7\"\n";
    let stack = detect(&repo(&[("Cargo.toml", cargo)])).unwrap();
    assert_eq!(stack.build_target.as_deref(), Some("billing-worker"));
    // Copied out of the cache mount, then into the runtime stage.
    assert!(
        stack
            .dockerfile()
            .contains("cp target/release/billing-worker /out/billing-worker"),
        "{}",
        stack.dockerfile()
    );

    // `name` also appears under `[[bin]]` and in dependency tables; only
    // the one under `[package]` is the crate's own.
    let tricky = "[package]\nname = \"gateway\"\n\n[[bin]]\nname = \"helper\"\n";
    assert_eq!(
        detect(&repo(&[("Cargo.toml", tricky)]))
            .unwrap()
            .build_target
            .as_deref(),
        Some("gateway")
    );
}

#[test]
fn the_detected_runtime_version_is_the_one_the_image_is_built_on() {
    let node = detect(&repo(&[("package.json", NEST_PACKAGE), (".nvmrc", "20")]))
        .unwrap()
        .dockerfile();
    assert!(node.contains("FROM node:20-alpine"), "{node}");

    let go = detect(&repo(&[("go.mod", "module x\n\ngo 1.21\n")]))
        .unwrap()
        .dockerfile();
    assert!(go.contains("FROM golang:1.21-alpine"), "{go}");

    // Silent repositories get a current default rather than nothing.
    let default = detect(&repo(&[("package.json", NEST_PACKAGE)]))
        .unwrap()
        .dockerfile();
    assert!(default.contains("FROM node:22-alpine"), "{default}");
}

// ---------------------------------------------------------------------------
// monorepos
// ---------------------------------------------------------------------------

/// The shape a modern JS monorepo actually has: a root that ships nothing,
/// two deployable apps, and a shared library that is not one.
fn turborepo() -> RepoManifest {
    repo(&[
        (
            "package.json",
            r#"{"name":"acme","private":true,"workspaces":["apps/*","packages/*"]}"#,
        ),
        ("package-lock.json", "{}"),
        ("turbo.json", r#"{"tasks":{"build":{}}}"#),
        (
            "apps/api/package.json",
            r#"{"name":"@acme/api","dependencies":{"@nestjs/core":"^10.0.0","@acme/shared":"workspace:*"}}"#,
        ),
        (
            "apps/web/package.json",
            r#"{"name":"@acme/web","dependencies":{"next":"14.2.0"}}"#,
        ),
        (
            "packages/shared/package.json",
            r#"{"name":"@acme/shared","main":"index.ts","devDependencies":{"typescript":"^5.4.0"}}"#,
        ),
    ])
}

#[test]
fn a_turborepo_is_recognised_with_its_deployable_apps() {
    let mono = detect_monorepo(&turborepo()).unwrap();

    assert_eq!(mono.kind, WorkspaceKind::PackageJson);
    assert_eq!(mono.task_runner.as_deref(), Some("turborepo"));

    let paths: Vec<_> = mono.deployable.iter().map(|w| w.path.as_str()).collect();
    // The shared library is a workspace member but not an environment:
    // nothing starts it, other packages import it.
    assert_eq!(paths, vec!["apps/api", "apps/web"]);

    let api = &mono.deployable[0];
    assert_eq!(api.framework, Framework::NestJs);
    // The filter takes the package name, and it differs from the directory
    // often enough that guessing would be wrong.
    assert_eq!(api.name, "@acme/api");
    assert_eq!(api.port, 3000);
    assert_eq!(mono.deployable[1].framework, Framework::NextJs);
}

#[test]
fn a_pnpm_workspace_is_recognised_by_its_own_declaration() {
    let mono = detect_monorepo(&repo(&[
        ("package.json", r#"{"name":"acme","private":true}"#),
        ("pnpm-workspace.yaml", "packages:\n  - 'apps/*'\n"),
        ("pnpm-lock.yaml", ""),
        (
            "apps/api/package.json",
            r#"{"name":"api","dependencies":{"fastify":"^4"}}"#,
        ),
    ]))
    .unwrap();
    assert_eq!(mono.kind, WorkspaceKind::Pnpm);
    assert_eq!(mono.deployable.len(), 1);
}

#[test]
fn an_ordinary_repository_is_not_a_monorepo() {
    assert!(detect_monorepo(&repo(&[("package.json", NEST_PACKAGE)])).is_none());
    assert!(detect_monorepo(&repo(&[("go.mod", "module x\n\ngo 1.23\n")])).is_none());
}

#[test]
fn a_workspace_build_installs_from_the_root_and_filters_to_the_target() {
    // The failure this prevents: a member imports `@acme/shared`, whose
    // resolution lives in the root lockfile. Installing from inside
    // `apps/api` finds neither the sibling nor the lock, and the build dies
    // on an import the developer can see working locally.
    let mono = detect_monorepo(&turborepo()).unwrap();
    let api = &mono.deployable[0];
    let dockerfile = mono.dockerfile(api, Some("22"), PackageManager::Npm, true);

    assert!(dockerfile.contains("WORKDIR /repo"), "{dockerfile}");
    assert!(
        dockerfile.contains("--workspace @acme/api"),
        "the build is not scoped to the target package\n{dockerfile}"
    );
    // Manifests before source, or the install layer dies on every commit —
    // which in a monorepo is where all the build time is.
    let install = dockerfile.find("npm ci").unwrap();
    assert!(
        install < dockerfile.find("COPY . .").unwrap(),
        "{dockerfile}"
    );
    // The root manifests have to be in the image for the install to resolve
    // anything at all.
    assert!(
        dockerfile.contains("COPY package.json *lock*"),
        "{dockerfile}"
    );
    assert!(
        dockerfile.contains("COPY apps/api/package.json"),
        "{dockerfile}"
    );
}

#[test]
fn each_workspace_manager_scopes_its_install_its_own_way() {
    // Getting this wrong installs the whole monorepo or none of the
    // siblings, and both fail in ways that point at the wrong thing.
    let pnpm = detect_monorepo(&repo(&[
        ("package.json", r#"{"name":"acme"}"#),
        ("pnpm-workspace.yaml", "packages:\n  - 'apps/*'\n"),
        (
            "apps/api/package.json",
            r#"{"name":"@acme/api","dependencies":{"@nestjs/core":"^10"}}"#,
        ),
    ]))
    .unwrap();
    let df = pnpm.dockerfile(&pnpm.deployable[0], None, PackageManager::Pnpm, true);
    // `...` is pnpm's "and everything it depends on" — without it the
    // siblings are not installed.
    assert!(df.contains("--filter @acme/api..."), "{df}");

    let mono = detect_monorepo(&turborepo()).unwrap();
    let yarn = mono.dockerfile(&mono.deployable[0], None, PackageManager::Yarn, true);
    assert!(
        yarn.contains("yarn workspace @acme/api run build"),
        "{yarn}"
    );
}

#[test]
fn a_workspace_member_ships_the_tree_it_needs_to_run() {
    // Learned by deploying one: copying `node_modules` plus the single
    // package produced an image that started and died on MODULE_NOT_FOUND.
    // A workspace links siblings into `node_modules` as symlinks pointing
    // back at `packages/*`, so leaving those behind breaks an import the
    // developer can see working locally.
    let mono = detect_monorepo(&turborepo()).unwrap();
    let api = mono.dockerfile(&mono.deployable[0], None, PackageManager::Npm, true);
    assert!(api.contains("COPY --from=build /repo ./"), "{api}");
    assert!(
        api.contains("CMD [\"node\", \"apps/api/dist/main.js\"]"),
        "{api}"
    );

    // A SPA is the exception: once built it is files, and needs neither
    // Node nor the workspace.
    let web = &mono.deployable[1];
    let spa = Workspace {
        framework: Framework::SinglePageApp,
        ..web.clone()
    };
    let df = mono.dockerfile(&spa, None, PackageManager::Npm, true);
    assert!(df.contains("FROM nginx:alpine"), "{df}");
    assert!(
        !df.split("FROM nginx:alpine")
            .nth(1)
            .unwrap()
            .contains("node_modules"),
        "{df}"
    );
}

// ---------------------------------------------------------------------------
// build speed
// ---------------------------------------------------------------------------

#[test]
fn every_generated_build_caches_its_dependency_downloads() {
    // The layer cache only survives while nothing above it changes: touch a
    // lockfile and the whole install runs again, re-downloading every
    // package. A cache mount is not part of a layer, so it survives that —
    // and it is shared between branches, which is where a preview
    // environment's build time goes.
    let cases = [
        (
            repo(&[("package.json", NEST_PACKAGE), ("package-lock.json", "")]),
            "/root/.npm",
        ),
        (
            repo(&[("package.json", NEST_PACKAGE), ("pnpm-lock.yaml", "")]),
            "pnpm/store",
        ),
        (
            repo(&[("package.json", NEST_PACKAGE), ("yarn.lock", "")]),
            "yarn",
        ),
        (repo(&[("go.mod", "module x\n\ngo 1.23\n")]), "/go/pkg/mod"),
        (
            repo(&[("requirements.txt", "fastapi\n")]),
            "/root/.cache/pip",
        ),
        (
            repo(&[(
                "Cargo.toml",
                "[package]\nname = \"svc\"\n\n[dependencies]\naxum = \"0.7\"\n",
            )]),
            "cargo/registry",
        ),
    ];
    for (manifest, expected) in cases {
        let stack = detect(&manifest).unwrap();
        let dockerfile = stack.dockerfile();
        assert!(
            dockerfile.contains("--mount=type=cache"),
            "{}: nothing is cached between builds\n{dockerfile}",
            stack.label()
        );
        assert!(
            dockerfile.contains(expected),
            "{}: expected a cache for `{expected}`\n{dockerfile}",
            stack.label()
        );
    }
}

#[test]
fn python_ships_a_virtualenv_and_not_a_build_toolchain() {
    // Installing in place put pip, setuptools and whatever a wheel needed
    // to compile into the running image, none of which the process uses.
    let dockerfile = detect(&repo(&[("requirements.txt", "fastapi\nuvicorn\n")]))
        .unwrap()
        .dockerfile();
    assert_eq!(
        dockerfile.matches("FROM python:").count(),
        2,
        "{dockerfile}"
    );
    assert!(
        dockerfile.contains("python -m venv /opt/venv"),
        "{dockerfile}"
    );
    assert!(
        dockerfile.contains("COPY --from=build /opt/venv /opt/venv"),
        "the venv never reaches the runtime stage\n{dockerfile}"
    );
    // The runtime stage has to find the venv's interpreter first, or it
    // runs the system one and none of the dependencies exist.
    let runtime = dockerfile.split("FROM python:").nth(2).unwrap();
    assert!(runtime.contains("/opt/venv/bin:$PATH"), "{runtime}");

    // The point of the split: `python:*-slim` has no compiler, so any
    // dependency without a wheel for the platform failed to install at all.
    // The toolchain goes in the build stage and stays there.
    let build = dockerfile.split("FROM python:").nth(1).unwrap();
    assert!(build.contains("build-essential"), "{build}");
    assert!(
        !runtime.contains("build-essential"),
        "the compiler shipped in the running image\n{runtime}"
    );
    // Postgres headers to build against, the runtime library to link
    // against — and never the headers in the running image. Oxid
    // provisions a database per branch, so this is the one native
    // dependency it can predict.
    assert!(build.contains("libpq-dev"), "{build}");
    assert!(runtime.contains("libpq5"), "{runtime}");
    assert!(!runtime.contains("libpq-dev"), "{runtime}");
}

#[test]
fn pip_is_not_told_to_throw_its_cache_away() {
    // `--no-cache-dir` is right without BuildKit — it keeps the cache out of
    // the image layer. With a cache mount the cache is not in a layer at
    // all, so the flag only discards work between builds.
    let dockerfile = detect(&repo(&[("requirements.txt", "fastapi\n")]))
        .unwrap()
        .dockerfile();
    assert!(!dockerfile.contains("--no-cache-dir"), "{dockerfile}");
}

#[test]
fn the_rust_binary_survives_its_own_target_cache() {
    // The trap: `target` as a cache mount is not part of any layer, so a
    // later `COPY --from=build /src/target/...` finds nothing. The binary
    // has to be copied out inside the same `RUN`, while the mount exists.
    let dockerfile = detect(&repo(&[(
        "Cargo.toml",
        "[package]\nname = \"billing\"\n\n[dependencies]\naxum = \"0.7\"\n",
    )]))
    .unwrap()
    .dockerfile();

    assert!(dockerfile.contains("target=/src/target"), "{dockerfile}");
    assert!(
        dockerfile.contains("cp target/release/billing /out/billing"),
        "the artifact is never taken out of the cache mount\n{dockerfile}"
    );
    assert!(
        dockerfile.contains("COPY --from=build /out/billing"),
        "still copies from a path the cache mount owns\n{dockerfile}"
    );
    assert!(
        !dockerfile.contains("COPY --from=build /src/target"),
        "{dockerfile}"
    );
    // Cargo locks a target directory anyway; saying so makes two concurrent
    // branch builds queue explicitly rather than surprisingly.
    assert!(dockerfile.contains("sharing=locked"), "{dockerfile}");
}

#[test]
fn the_images_a_build_needs_are_knowable_before_it_runs() {
    // Registration is when Oxid learns what a project is built with, and it
    // is usually a long time before the first push. Naming the images here
    // is what lets that gap be spent on the download instead of the person
    // watching their first deploy.
    let spa = detect(&repo(&[("package.json", VITE_PACKAGE)])).unwrap();
    // A SPA builds with Node and ships behind nginx — both, or the second
    // stage still stalls.
    assert_eq!(spa.base_images(), vec!["node:22-alpine", "nginx:alpine"]);

    let nest = detect(&repo(&[("package.json", NEST_PACKAGE), (".nvmrc", "20")])).unwrap();
    assert_eq!(nest.base_images(), vec!["node:20-alpine"]);

    let go = detect(&repo(&[("go.mod", "module x\n\ngo 1.21\n")])).unwrap();
    assert_eq!(
        go.base_images(),
        vec!["golang:1.21-alpine", "alpine:latest"]
    );

    // Every name must be one the Dockerfile actually uses: two readings of
    // the same fact drift, and this is the one nothing else would catch.
    for manifest in [
        repo(&[("package.json", VITE_PACKAGE)]),
        repo(&[("package.json", NEST_PACKAGE)]),
        repo(&[("go.mod", "module x\n\ngo 1.23\n")]),
        repo(&[("requirements.txt", "fastapi\n")]),
        repo(&[("Cargo.toml", "[package]\nname = \"svc\"\n")]),
        repo(&[("index.html", "<h1>hi</h1>")]),
    ] {
        let stack = detect(&manifest).unwrap();
        let dockerfile = stack.dockerfile();
        for image in stack.base_images() {
            assert!(
                dockerfile.contains(&format!("FROM {image}")),
                "{}: pre-fetches `{image}`, which its Dockerfile never uses\n{dockerfile}",
                stack.label()
            );
        }
    }
}

#[test]
fn a_next_app_without_a_public_directory_still_builds() {
    // `public/` is optional in Next.js and `COPY` fails outright on a
    // missing source, so an app with no static assets failed at the last
    // step of a slow build. Found by deploying one.
    let dockerfile = detect(&repo(&[(
        "package.json",
        r#"{"dependencies":{"next":"14.2.15"},"scripts":{"build":"next build"}}"#,
    )]))
    .unwrap()
    .dockerfile();
    assert!(dockerfile.contains("mkdir -p public"), "{dockerfile}");
    let mkdir = dockerfile.find("mkdir -p public").unwrap();
    let copy = dockerfile.find("/app/public").unwrap();
    assert!(
        mkdir < copy,
        "the directory is created after it is copied\n{dockerfile}"
    );
}

// ---------------------------------------------------------------------------
// the wider web
// ---------------------------------------------------------------------------

#[test]
fn the_javascript_meta_frameworks_are_told_apart() {
    // Order matters here: a Nuxt app has `vite`, a SvelteKit app has
    // `svelte`, and matching the generic one first would build every one of
    // them as a static site with nothing to serve.
    for (dep, expected, port) in [
        ("nuxt", Framework::Nuxt, 3000),
        ("@sveltejs/kit", Framework::SvelteKit, 3000),
        ("@remix-run/node", Framework::Remix, 3000),
    ] {
        let package = format!(
            r#"{{"devDependencies":{{"vite":"^5.0.0"}},"dependencies":{{"{dep}":"^1.0.0"}}}}"#
        );
        let stack = detect(&repo(&[("package.json", &package)])).unwrap();
        assert_eq!(stack.framework, expected, "{dep}");
        assert_eq!(stack.port, port, "{dep}");
    }
}

#[test]
fn astro_is_static_until_something_gives_it_a_server() {
    // The two need entirely different images: one is nginx holding files,
    // the other is a Node process.
    let stat = detect(&repo(&[(
        "package.json",
        r#"{"dependencies":{"astro":"^4"}}"#,
    )]))
    .unwrap();
    assert_eq!(stat.framework, Framework::SinglePageApp);
    assert!(stat.dockerfile().contains("FROM nginx:alpine"));

    let ssr = detect(&repo(&[(
        "package.json",
        r#"{"dependencies":{"astro":"^4","@astrojs/node":"^8"}}"#,
    )]))
    .unwrap();
    assert_eq!(ssr.framework, Framework::Astro);
    let dockerfile = ssr.dockerfile();
    assert!(dockerfile.contains("dist/server/entry.mjs"), "{dockerfile}");
    assert!(!dockerfile.contains("nginx"), "{dockerfile}");
}

#[test]
fn each_meta_framework_runs_the_entry_point_it_actually_emits() {
    // None of these are guessable, and getting one wrong fails at the end
    // of a slow build with a path nobody in the project ever wrote.
    for (dep, entry) in [
        ("nuxt", ".output/server/index.mjs"),
        ("@sveltejs/kit", "build/index.js"),
        ("@remix-run/node", "build/server/index.js"),
    ] {
        let package = format!(r#"{{"dependencies":{{"{dep}":"^1.0.0"}}}}"#);
        let dockerfile = detect(&repo(&[("package.json", &package)]))
            .unwrap()
            .dockerfile();
        assert!(
            dockerfile.contains(&format!("CMD [\"node\", \"{entry}\"]")),
            "{dep}: {dockerfile}"
        );
    }

    // Nuxt's Nitro output bundles its dependencies; the others still
    // resolve from `node_modules`, so the runtime stage has to install.
    let nuxt = detect(&repo(&[(
        "package.json",
        r#"{"dependencies":{"nuxt":"^3"}}"#,
    )]))
    .unwrap()
    .dockerfile();
    let nuxt_runtime = nuxt.rsplit("FROM ").next().unwrap();
    assert!(!nuxt_runtime.contains("install"), "{nuxt}");

    let kit = detect(&repo(&[(
        "package.json",
        r#"{"dependencies":{"@sveltejs/kit":"^2"}}"#,
    )]))
    .unwrap()
    .dockerfile();
    let kit_runtime = kit.rsplit("FROM ").next().unwrap();
    assert!(kit_runtime.contains("install"), "{kit}");
}

#[test]
fn laravel_and_symfony_are_recognised_and_served_from_public() {
    // Serving the repository root instead of `public/` exposes `.env` and
    // every source file to anyone who can reach the environment.
    for (package, expected) in [
        ("laravel/framework", Framework::Laravel),
        ("symfony/framework-bundle", Framework::Symfony),
    ] {
        let composer = format!(r#"{{"require":{{"php":"^8.2","{package}":"^11.0"}}}}"#);
        let stack = detect(&repo(&[
            ("composer.json", &composer),
            ("composer.lock", ""),
        ]))
        .unwrap();
        assert_eq!(stack.runtime, Runtime::Php, "{package}");
        assert_eq!(stack.framework, expected, "{package}");
        assert_eq!(stack.runtime_version.as_deref(), Some("8.2"), "{package}");
        let dockerfile = stack.dockerfile();
        assert!(dockerfile.contains("FROM php:8.2-cli"), "{dockerfile}");
        assert!(dockerfile.contains("-t\", \"public"), "{dockerfile}");
        assert!(
            dockerfile.contains("--no-dev"),
            "dev dependencies ship\n{dockerfile}"
        );
        // The headers compile the extension and are never needed again.
        // Dropping them in the same layer is what keeps them out of the
        // image — a later purge would only add a layer that hides them.
        assert!(
            dockerfile.contains("purge -y --auto-remove libpq-dev"),
            "{dockerfile}"
        );
        assert!(dockerfile.contains("libpq5"), "{dockerfile}");
    }
}

#[test]
fn rails_is_recognised_and_a_plain_gemfile_is_not() {
    let gemfile = "source 'https://rubygems.org'\nruby '3.3.4'\ngem 'rails', '~> 7.1'\n";
    let stack = detect(&repo(&[("Gemfile", gemfile), ("Gemfile.lock", "")])).unwrap();
    assert_eq!(stack.runtime, Runtime::Ruby);
    assert_eq!(stack.framework, Framework::Rails);
    assert_eq!(stack.runtime_version.as_deref(), Some("3.3.4"));
    let dockerfile = stack.dockerfile();
    assert!(dockerfile.contains("FROM ruby:3.3.4-slim"), "{dockerfile}");
    // Rails will not boot in production without these, and a preview
    // environment has none of its own.
    assert!(dockerfile.contains("RAILS_LOG_TO_STDOUT"), "{dockerfile}");
    assert!(dockerfile.contains("-b\", \"0.0.0.0"), "{dockerfile}");

    // A Gemfile describes a library as often as a web application.
    assert!(detect(&repo(&[("Gemfile", "source 'x'\ngem 'rspec'\n")])).is_none());
}

#[test]
fn spring_boot_builds_with_the_wrapper_the_project_checked_in() {
    // Using a system Maven or Gradle is how a build that works locally
    // fails here: the version a project builds with is checked into it.
    let pom = "<project><properties><java.version>21</java.version></properties>\n               <dependency><artifactId>spring-boot-starter-web</artifactId></dependency></project>";
    let maven = detect(&repo(&[("pom.xml", pom)])).unwrap();
    assert_eq!(maven.runtime, Runtime::Java);
    assert_eq!(maven.framework, Framework::SpringBoot);
    assert_eq!(maven.runtime_version.as_deref(), Some("21"));
    let dockerfile = maven.dockerfile();
    assert!(dockerfile.contains("./mvnw"), "{dockerfile}");
    // JRE at runtime: the compiler is hundreds of megabytes the service
    // never uses.
    assert!(
        dockerfile.contains("FROM eclipse-temurin:21-jre"),
        "{dockerfile}"
    );
    assert!(
        !dockerfile.rsplit("FROM ").next().unwrap().contains("jdk"),
        "{dockerfile}"
    );

    let gradle = detect(&repo(&[(
        "build.gradle",
        "plugins { id 'org.springframework.boot' version '3.3.0' }\njava { sourceCompatibility = 17 }",
    )]))
    .unwrap();
    assert_eq!(gradle.runtime_version.as_deref(), Some("17"));
    assert!(
        gradle.dockerfile().contains("./gradlew"),
        "{}",
        gradle.dockerfile()
    );

    // A JVM build file with no web framework is a library or a CLI.
    assert!(detect(&repo(&[("pom.xml", "<project></project>")])).is_none());
}

#[test]
fn a_dotnet_project_runs_the_assembly_its_file_is_named_after() {
    // `dotnet publish` names the DLL after the project file, so the wrong
    // name is a container that starts and immediately exits.
    let stack = detect(&repo(&[(
        "Billing.Api.csproj",
        "<Project Sdk=\"Microsoft.NET.Sdk.Web\" />",
    )]))
    .unwrap();
    assert_eq!(stack.runtime, Runtime::DotNet);
    assert_eq!(stack.build_target.as_deref(), Some("Billing.Api"));
    let dockerfile = stack.dockerfile();
    assert!(
        dockerfile.contains("CMD [\"dotnet\", \"Billing.Api.dll\"]"),
        "{dockerfile}"
    );
    // Kestrel binds to localhost by default, which from outside the
    // container is nothing at all.
    assert!(
        dockerfile.contains("ASPNETCORE_URLS=http://0.0.0.0:8080"),
        "{dockerfile}"
    );
    // Runtime image, not the SDK.
    assert!(dockerfile.contains("dotnet/aspnet:8.0"), "{dockerfile}");
}

#[test]
fn node_still_wins_over_a_language_that_only_tools_the_repository() {
    // A Node service with a Gemfile for its docs tooling, or a composer.json
    // for a linter, still deploys as Node. Detection order is what pins it,
    // and this is the test that would catch reordering the list.
    let stack = detect(&repo(&[
        ("package.json", NEST_PACKAGE),
        ("Gemfile", "source 'x'\ngem 'rails'\n"),
        (
            "composer.json",
            r#"{"require":{"laravel/framework":"^11"}}"#,
        ),
    ]))
    .unwrap();
    assert_eq!(stack.runtime, Runtime::Node);
}
