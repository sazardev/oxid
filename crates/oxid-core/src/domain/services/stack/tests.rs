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
