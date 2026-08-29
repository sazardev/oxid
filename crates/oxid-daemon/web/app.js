// Oxid dashboard logic. No build step, no framework beyond Alpine.js
// (vendored in vendor/alpine.min.js) — this file is loaded as a plain
// classic script and defines the `dashboard()` factory Alpine looks up
// from `x-data="dashboard()"` in index.html.
//
// This is a real multi-page app, not a single view with modals: every
// screen (environment detail, project detail, secrets, queue, audit,
// admin) has its own URL under `/ui/...`, deep-linkable and shareable, with
// filters reflected as query params. `api.rs`'s `router()` falls back to
// serving `index.html` for any unmatched GET so a hard refresh on a nested
// path still works — this file reads `location.pathname`/`location.search`
// on load and on every `popstate`/`go()` to decide what to render.
const ROUTES = [
  { name: "environment", re: /^\/ui\/environments\/(\d+)\/?$/, keys: ["id"] },
  { name: "projectSecrets", re: /^\/ui\/projects\/(\d+)\/secrets\/?$/, keys: ["id"] },
  { name: "project", re: /^\/ui\/projects\/(\d+)\/?$/, keys: ["id"] },
  { name: "projects", re: /^\/ui\/projects\/?$/, keys: [] },
  { name: "queue", re: /^\/ui\/queue\/?$/, keys: [] },
  { name: "audit", re: /^\/ui\/audit\/?$/, keys: [] },
  { name: "secrets", re: /^\/ui\/secrets\/?$/, keys: [] },
  { name: "admin", re: /^\/ui\/admin\/?$/, keys: [] },
  { name: "onboarding", re: /^\/ui\/onboarding\/?$/, keys: [] },
  { name: "environments", re: /^\/(ui\/environments)?\/?$/, keys: [] },
];

function resolveRoute(pathname) {
  for (const r of ROUTES) {
    const m = r.re.exec(pathname);
    if (m) {
      const params = {};
      r.keys.forEach((key, i) => {
        params[key] = Number(m[i + 1]);
      });
      return { name: r.name, params };
    }
  }
  return { name: "environments", params: {} };
}

function dashboard() {
  return {
    apiBase: "",
    token: "",
    online: false,
    authError: false,
    connError: false,
    notice: "",
    route: { name: "environments", params: {} },
    query: {},
    projects: [],
    stats: {},
    queue: [],
    audit: [],
    // environment id -> {branch, projectId, projectName}, for EVERY
    // environment including destroyed/failed ones (unlike `projects[].
    // environments`, which only keeps the current live deploy per branch) —
    // lets the audit trail show which branch/project a `BUILD_FAILED`
    // event was actually for instead of just an opaque environment id.
    envIndex: {},
    tokens: [],
    newTokenName: "",
    deployBranch: "",
    projectSettingsForm: { pause_after: "", destroy_after: "" },
    _settingsFormProjectId: null,
    logLines: [],
    historyEvents: [],
    secretsList: [],
    secretForm: { name: "", scope: "global", value: "", branch: "" },
    filterProject: "",
    filterState: "",
    filterQuery: "",
    auditLimit: 50,
    auditQuery: "",
    refreshIntervalSecs: 5,
    // Active locale. Every `t()` call reads it, which is what makes Alpine
    // re-render the whole UI when the switcher changes it — the catalog
    // itself is static, the reactivity lives here.
    locale: window.OxidI18n.detect(),
    locales: window.OxidI18n.available(),
    confirmModal: { open: false, message: "", resolve: null },
    _logAbort: null,
    _timer: null,
    // Public, pre-auth readiness probe (`GET /api/v1/setup/status`) — the
    // onboarding wizard and the first-visit auto-redirect both read it.
    setupStatus: null,
    wizard: {
      step: 1,
      tokenInput: "",
      checkingToken: false,
      generatingToken: false,
      infra: null,
      infraLoading: false,
      fixingInfra: false,
      infraError: "",
      projectMode: "url",
      repoUrl: "",
      gitToken: "",
      repoDir: "",
      registering: false,
      deployBranch: "main",
      deploying: false,
      deployState: "",
      deployMessage: "",
      projectId: null,
      projectName: "",
      envId: null,
      provider: "github",
      webhookSecret: null,
      webhookSecretMissing: false,
      copied: "",
    },

    init() {
      this.applyLocale();
      this.token = localStorage.getItem("oxid_token") || "";
      window.addEventListener("popstate", () => this.onRouteChange());
      this.onRouteChange();
      this._timer = setInterval(() => this.refreshCurrentPage(), this.refreshIntervalSecs * 1000);
      this.loadSetupStatus().then(() => this.maybeStartOnboarding());
    },

    // ------------------------------------------------------------------
    // i18n
    // ------------------------------------------------------------------

    /**
     * Translates `key`, substituting `{placeholder}` values from `params`.
     *
     * Reads `this.locale` on every call rather than closing over it, so
     * Alpine records the dependency and re-evaluates each binding when the
     * switcher changes language — no reload, no manual re-render.
     */
    t(key, params) {
      return window.OxidI18n.translate(this.locale, key, params);
    },

    /** Switches language and remembers the choice. */
    setLocale(locale) {
      this.locale = locale;
      window.OxidI18n.persist(locale);
      this.applyLocale();
    },

    /**
     * Propagates the locale to the parts of the page Alpine does not own:
     * `<html lang>` (screen readers, hyphenation, spellcheck) and the
     * document title, neither of which lives under `x-data`.
     */
    applyLocale() {
      document.documentElement.lang = this.locale;
      document.title = this.t("app.title");
    },

    /**
     * A state's display label. States travel over the API as stable
     * identifiers (`build_failed`); only their rendering is translated, so
     * filters and CSS classes keep matching the identifier.
     */
    stateLabel(state) {
      return this.t(`state.${state}`);
    },

    // ------------------------------------------------------------------
    // router
    // ------------------------------------------------------------------

    go(path) {
      if (path === location.pathname + location.search) {
        return;
      }
      history.pushState({}, "", path);
      this.onRouteChange();
    },

    setQuery(partial) {
      const url = new URL(location.href);
      for (const [k, v] of Object.entries(partial)) {
        if (v === null || v === undefined || v === "") {
          url.searchParams.delete(k);
        } else {
          url.searchParams.set(k, v);
        }
      }
      history.replaceState({}, "", url);
      this.onRouteChange();
    },

    onRouteChange() {
      const url = new URL(location.href);
      this.route = resolveRoute(url.pathname);
      this.query = Object.fromEntries(url.searchParams);
      this.filterProject = this.query.project ?? "";
      this.filterState = this.query.state ?? "";
      this.filterQuery = this.query.q ?? "";
      this.auditLimit = Number(this.query.limit ?? 50);
      this.auditQuery = this.query.q ?? "";
      this.closeLogStream();
      this.loadForRoute();
    },

    async refreshCurrentPage() {
      await this.loadForRoute();
    },

    // ------------------------------------------------------------------
    // data loading — dispatches on the current route, always refreshes the
    // global stats bar too so it stays live regardless of which page is open
    // ------------------------------------------------------------------

    async loadForRoute() {
      try {
        this.stats = await this.apiGet("/api/v1/stats");
        this.online = true;
        this.connError = false;
      } catch (err) {
        this.online = false;
        if (err.message !== "unauthorized") {
          this.connError = true;
        }
      }
      this.tokens = (await this.apiGetQuiet("/api/v1/tokens")) ?? [];

      // Not wrapped per-route above this line — an exception here must
      // never silently leave a page showing stale/empty data with no
      // indication anything went wrong (a real bug: a project's own
      // secrets briefly rendered empty because `loadProjectsWithEnvironments`
      // rejected higher up and neither the projects list nor the secrets
      // fetch that came after it in the same route case ever ran).
      try {
        switch (this.route.name) {
          case "environments":
          case "project":
          case "environment":
          case "projects":
            await this.loadProjectsWithEnvironments();
            break;
          case "projectSecrets":
            await this.loadProjectsWithEnvironments();
            await this.reloadSecrets();
            break;
          case "secrets":
            await this.reloadSecrets();
            break;
          case "queue":
            await this.loadProjectsWithEnvironments();
            this.queue = (await this.apiGetQuiet("/api/v1/queue")) ?? [];
            break;
          case "audit":
            // Needed to resolve `event.environment_id` back to a branch
            // name via `envIndex` for display — without it every row in
            // the audit trail just showed an opaque `#4` (found live: the
            // user couldn't tell which branch a `BUILD_FAILED` was for).
            await this.loadProjectsWithEnvironments();
            await this.loadAudit();
            break;
          case "admin":
            break;
          case "onboarding":
            await this.loadSetupStatus();
            break;
        }

        if (this.route.name === "environment") {
          this.historyEvents =
            (await this.apiGetQuiet(`/api/v1/environments/${this.route.params.id}/audit`)) ?? [];
          if (this.query.tab === "logs") {
            this.openLogStream(this.route.params.id);
          }
        }
      } catch (err) {
        if (err.message !== "unauthorized") {
          this.showNotice(this.t("notice.loadFailed", { error: err.message }));
        }
      }
    },

    async loadAudit() {
      const limit = this.auditLimit || 50;
      this.audit = (await this.apiGetQuiet(`/api/v1/audit?limit=${limit}`)) ?? [];
    },

    // ------------------------------------------------------------------
    // http helpers
    // ------------------------------------------------------------------

    authHeaders() {
      const headers = {};
      if (this.token) {
        headers.Authorization = `Bearer ${this.token}`;
      }
      // Tells the daemon which language to answer in. Harmless against a
      // daemon that ignores it, and it means a message the API produces
      // arrives already translated rather than needing a second catalog
      // here that would have to be kept in step with the server's wording.
      headers["Accept-Language"] = this.locale;
      return headers;
    },

    saveToken() {
      localStorage.setItem("oxid_token", this.token);
      this.loadForRoute();
    },

    showNotice(message) {
      this.notice = message;
    },

    confirmDialog(message) {
      return new Promise((resolve) => {
        this.confirmModal = { open: true, message, resolve };
      });
    },

    resolveConfirm(result) {
      this.confirmModal.resolve?.(result);
      this.confirmModal.open = false;
    },

    async apiGet(path) {
      const res = await fetch(this.apiBase + path, {
        headers: this.authHeaders(),
        cache: "no-store",
      });
      if (res.status === 401 || res.status === 403) {
        this.authError = true;
        throw new Error("unauthorized");
      }
      this.authError = false;
      if (!res.ok) {
        throw new Error(`${res.status}`);
      }
      return res.json();
    },

    // Same as `apiGet`, but for calls that are allowed to fail quietly
    // (e.g. the token list, which 403s for a valid non-master operator
    // token — that isn't a reason to show the "check your token" banner).
    async apiGetQuiet(path) {
      try {
        const res = await fetch(this.apiBase + path, {
          headers: this.authHeaders(),
          cache: "no-store",
        });
        if (!res.ok) {
          return null;
        }
        return await res.json();
      } catch {
        return null;
      }
    },

    async apiSend(method, path, body) {
      const opts = { method, headers: this.authHeaders(), cache: "no-store" };
      if (body !== undefined) {
        opts.headers["Content-Type"] = "application/json";
        opts.body = JSON.stringify(body);
      }
      const res = await fetch(this.apiBase + path, opts);
      if (res.status === 401 || res.status === 403) {
        this.authError = true;
        throw new Error("unauthorized");
      }
      this.authError = false;
      if (!res.ok) {
        const text = await res.text().catch(() => "");
        let message = text || `${res.status}`;
        try {
          message = JSON.parse(text).error ?? message;
        } catch {
          // not JSON — use the raw text as-is
        }
        throw new Error(message);
      }
      return res;
    },

    async loadProjectsWithEnvironments() {
      const projects = await this.apiGet("/api/v1/projects");
      const envIndex = {};
      for (const project of projects) {
        const envs = await this.apiGet(`/api/v1/projects/${project.id}/environments`);
        for (const env of envs) {
          envIndex[env.id] = {
            branch: env.branch.name,
            projectId: project.id,
            projectName: project.name,
            createdAtMs: this.toEpochMs(env.created_at),
          };
        }
        project.environments = this.latestPerBranch(envs);
      }
      this.projects = projects;
      this.envIndex = envIndex;
    },

    // Resolves an audit event's `environment_id` back to the branch/project
    // it belongs to — works even for a `BUILD_FAILED`/destroyed environment
    // that never shows up in the live environments list, since `envIndex`
    // is built from the *unfiltered* per-project environment list.
    envRef(environmentId) {
      const ref = this.envIndex[environmentId];
      return ref ? `${ref.projectName}/${ref.branch}` : `#${environmentId}`;
    },

    // Historical deploys keep one row per past commit (for `oxid rollback`);
    // the dashboard only shows the current live one per branch, same as
    // `oxid status` does.
    latestPerBranch(envs) {
      const byBranch = new Map();
      for (const env of envs) {
        const key = env.branch.name;
        const existing = byBranch.get(key);
        if (!existing || env.id > existing.id) {
          byBranch.set(key, env);
        }
      }
      return Array.from(byBranch.values())
        .filter((e) => e.state !== "destroyed")
        .sort((a, b) => a.branch.name.localeCompare(b.branch.name));
    },

    // ------------------------------------------------------------------
    // derived view data
    // ------------------------------------------------------------------

    allEnvironments() {
      const rows = [];
      for (const project of this.projects) {
        for (const env of project.environments ?? []) {
          rows.push({ ...env, projectName: project.name, projectId: project.id });
        }
      }
      return rows;
    },

    filteredEnvironments() {
      return this.allEnvironments().filter((env) => {
        if (this.filterProject && String(env.projectId) !== String(this.filterProject)) {
          return false;
        }
        if (this.filterState && env.state !== this.filterState) {
          return false;
        }
        if (
          this.filterQuery &&
          !env.branch.name.toLowerCase().includes(this.filterQuery.toLowerCase())
        ) {
          return false;
        }
        return true;
      });
    },

    filteredAudit() {
      if (!this.auditQuery) {
        return this.audit;
      }
      const q = this.auditQuery.toLowerCase();
      return this.audit.filter(
        (e) =>
          e.kind.toLowerCase().includes(q) ||
          (e.detail ?? "").toLowerCase().includes(q) ||
          (e.operator ?? "").toLowerCase().includes(q) ||
          this.envRef(e.environment_id).toLowerCase().includes(q),
      );
    },

    currentProject() {
      return this.projects.find((p) => p.id === this.route.params.id) ?? null;
    },

    currentEnvironment() {
      return this.allEnvironments().find((e) => e.id === this.route.params.id) ?? null;
    },

    projectName(id) {
      return this.projects.find((p) => p.id === id)?.name ?? `#${id}`;
    },

    // Without Traefik (`OXID_DOCKER_NETWORK` unset), `env.url` is a
    // `branch.base-domain` hostname that only means something as a Traefik
    // `Host()` rule — it isn't reachable as a URL at all without DNS/hosts
    // pointing it somewhere. In that mode the real, directly-reachable
    // address is `env.host_port` — the host port Docker actually bound for
    // *this* environment (always dynamically chosen now, so a busy port
    // never blocks a deploy; see `ControlPlane::run_and_activate`), not the
    // project's static `[routing].port`, which is just the container's own
    // internal listening port and no longer says anything about the host
    // side.
    // `public_port` (when present) is the branch's stable address — bound
    // once by Oxid's own built-in zero-downtime proxy and reused across
    // every redeploy, unlike `host_port` which changes each time a new
    // container is cut over. Prefer it; fall back to `host_port` for
    // environments deployed before this existed.
    envAddressPort(env) {
      return env.public_port ?? env.host_port;
    },

    envLink(env) {
      if (this.stats.traefik_enabled) {
        return `http://${env.url}`;
      }
      const port = this.envAddressPort(env);
      return port
        ? `${location.protocol}//${location.hostname}:${port}/`
        : `http://${env.url}`;
    },

    envLinkLabel(env) {
      if (this.stats.traefik_enabled) {
        return env.url;
      }
      const port = this.envAddressPort(env);
      return port ? `${location.hostname}:${port}` : env.url;
    },

    hostMemoryLabel() {
      if (!this.stats.host_total_memory_bytes) {
        return "—";
      }
      const gb = this.stats.host_total_memory_bytes / 1_073_741_824;
      return `${gb.toFixed(1)}G / ${this.stats.host_cpu_count ?? "?"} cpu`;
    },

    // Renders an API timestamp in the viewer's own locale and timezone.
    //
    // The daemon sends RFC 3339, so this is a parse and a format rather than
    // the hand-rolled calendar arithmetic it replaces: timestamps used to
    // arrive as `time`'s positional array and were rendered `2026-day241
    // 15:20:45`, the second number being the ordinal day of the year. The
    // array branch is kept for one release so a newer dashboard still shows
    // something sane against an older daemon.
    formatTime(value) {
      const ms = this.toEpochMs(value);
      if (ms === null) {
        return String(value ?? "");
      }
      return new Intl.DateTimeFormat(this.locale, {
        dateStyle: "short",
        timeStyle: "medium",
      }).format(new Date(ms));
    },

    // Epoch milliseconds for an API timestamp, so push→deploy speed can be
    // measured. `null` for anything unparseable, which the callers render as
    // an em dash rather than `NaN`.
    toEpochMs(value) {
      if (typeof value === "string") {
        const ms = Date.parse(value);
        return Number.isNaN(ms) ? null : ms;
      }
      // Legacy `[year, ordinal_day, hour, min, sec, ...]`: `Date.UTC` with
      // day 1 of the year plus `ordinal - 1` days handles month boundaries
      // and leap years without reimplementing a calendar.
      if (Array.isArray(value)) {
        const [year, day, hour, min, sec] = value;
        return Date.UTC(year, 0, 1) + (day - 1) * 86_400_000 + hour * 3_600_000 + min * 60_000
          + sec * 1000;
      }
      return null;
    },

    // Human-readable elapsed time for a push→deploy duration, e.g. "1.4s"
    // or "340ms" — deploys are fast enough that anything coarser than a
    // second loses the number that actually matters here.
    formatDuration(ms) {
      if (ms === null || Number.isNaN(ms) || ms < 0) {
        return "—";
      }
      if (ms < 1000) {
        return `${ms}ms`;
      }
      return `${(ms / 1000).toFixed(1)}s`;
    },

    // Wall-clock time from an environment's `created_at` (`deploy_at`
    // creates that row *before* the git clone/build/run pipeline even
    // starts, so it's an accurate "push received" proxy) to a
    // `build_succeeded`/`build_failed` audit event closing it out — the
    // real push→live latency, computed entirely from data already
    // collected rather than adding new tracking. `null` for any other kind
    // of event (pause/wake/destroy/idle_timeout have no "duration").
    auditDuration(event) {
      if (event.kind !== "build_succeeded" && event.kind !== "build_failed") {
        return null;
      }
      const ref = this.envIndex[event.environment_id];
      if (!ref?.createdAtMs) {
        return null;
      }
      const finishedMs = this.toEpochMs(event.occurred_at);
      return finishedMs === null ? null : finishedMs - ref.createdAtMs;
    },

    // ------------------------------------------------------------------
    // onboarding wizard (`/ui/onboarding`) — a first-run checklist that
    // turns "docker compose up" into a working, deployed project: token →
    // infra → first project + deploy → webhooks → CLI. Everything it calls
    // already existed (infra/bootstrap, projects, deploy); the only
    // wizard-specific endpoint is the public `setup/status` probe.
    // ------------------------------------------------------------------

    async loadSetupStatus() {
      try {
        const res = await fetch(`${this.apiBase}/api/v1/setup/status`, { cache: "no-store" });
        this.setupStatus = await res.json();
      } catch {
        this.setupStatus = null;
      }
    },

    // Auto-redirects to the wizard on the very first visit to the home
    // page of a daemon this browser hasn't been set up against yet — but
    // never hijacks deep links into other views. Dismissed permanently by
    // finishing or skipping (localStorage `oxid_onboarded`).
    maybeStartOnboarding() {
      if (localStorage.getItem("oxid_onboarded") === "1") {
        return;
      }
      const path = location.pathname;
      if (path !== "/" && path !== "/ui/environments") {
        return;
      }
      const s = this.setupStatus;
      if (!s) {
        return; // unreachable daemon — the connError banner already explains
      }
      if (!s.auth_required || !this.token) {
        this.go("/ui/onboarding");
      }
    },

    wizardGo(step) {
      this.wizard.step = step;
      if (step === 2) {
        this.loadInfraStatus();
      }
      if (step === 4) {
        this.loadWebhookSecret();
      }
    },

    finishOnboarding(message) {
      localStorage.setItem("oxid_onboarded", "1");
      if (message) {
        this.showNotice(message);
      }
      this.go("/ui/environments");
    },

    async verifyToken() {
      const candidate = this.wizard.tokenInput.trim();
      if (!candidate) {
        return;
      }
      this.wizard.checkingToken = true;
      const previous = this.token;
      this.token = candidate;
      try {
        await this.apiGet("/api/v1/stats");
        this.authError = false;
        localStorage.setItem("oxid_token", candidate);
        this.showNotice(this.t("notice.tokenAccepted"));
        this.wizardGo(2);
      } catch {
        this.token = previous;
        localStorage.setItem("oxid_token", previous);
        this.showNotice(
          this.t("notice.tokenRejected"),
        );
      } finally {
        this.wizard.checkingToken = false;
      }
    },

    // Self-serves the auto-generated master token from the public
    // `GET /api/v1/setup/token` (see that handler's doc comment for why
    // this is safe: it only ever hands over the *auto-generated* value,
    // never one the operator set explicitly). Mirrors `oxid token
    // generate` on the CLI side.
    async generateToken() {
      this.wizard.generatingToken = true;
      try {
        const res = await fetch(`${this.apiBase}/api/v1/setup/token`, { cache: "no-store" });
        if (!res.ok) {
          throw new Error("no auto-generated token available");
        }
        const body = await res.json();
        this.token = body.token;
        localStorage.setItem("oxid_token", body.token);
        this.authError = false;
        this.showNotice(this.t("notice.tokenGenerated"));
        this.wizardGo(2);
      } catch {
        this.showNotice(
          this.t("notice.tokenGenerateFailed"),
        );
      } finally {
        this.wizard.generatingToken = false;
      }
    },

    async loadInfraStatus() {
      this.wizard.infraLoading = true;
      this.wizard.infraError = "";
      // 404 means direct-publish mode (no OXID_DOCKER_NETWORK) — a valid,
      // supported topology where there's simply nothing to bootstrap.
      this.wizard.infra = await this.apiGetQuiet("/api/v1/infra/status");
      this.wizard.infraLoading = false;
    },

    async fixInfra() {
      this.wizard.fixingInfra = true;
      this.wizard.infraError = "";
      try {
        const res = await this.apiSend("POST", "/api/v1/infra/bootstrap", {});
        this.wizard.infra = await res.json();
      } catch (err) {
        this.wizard.infraError =
          err.message === "unauthorized"
            ? this.t("notice.masterOnly")
            : err.message;
      } finally {
        this.wizard.fixingInfra = false;
      }
    },

    async registerFirstProject() {
      this.wizard.registering = true;
      this.wizard.deployState = "";
      this.wizard.deployMessage = "";
      try {
        const body =
          this.wizard.projectMode === "url"
            ? {
                repo_url: this.wizard.repoUrl.trim(),
                ...(this.wizard.gitToken.trim()
                  ? { git_token: this.wizard.gitToken.trim() }
                  : {}),
              }
            : { repo_dir: this.wizard.repoDir.trim() };
        const res = await this.apiSend("POST", "/api/v1/projects", body);
        const project = await res.json();
        this.wizard.projectId = project.id;
        this.wizard.projectName = project.name;
        this.showNotice(this.t("notice.registered", { name: project.name }));
        await this.deployFirstProject();
      } catch (err) {
        this.showNotice(this.t("notice.registerFailed", { error: err.message }));
      } finally {
        this.wizard.registering = false;
      }
    },

    async deployFirstProject() {
      const branch = this.wizard.deployBranch.trim() || "main";
      this.wizard.deploying = true;
      this.wizard.deployState = "building";
      try {
        await this.apiSend("POST", `/api/v1/projects/${this.wizard.projectId}/deploy`, {
          branch,
        });
        this.pollFirstDeploy(branch, 0);
      } catch (err) {
        this.wizard.deploying = false;
        this.wizard.deployState = "failed";
        this.wizard.deployMessage = err.message;
      }
    },

    // Polls until the first deploy lands (`running`), fails, or times out
    // (~3 min). Builds can legitimately take minutes on a cold host.
    async pollFirstDeploy(branch, attempt) {
      if (attempt > 90) {
        this.wizard.deploying = false;
        this.wizard.deployState = "timeout";
        this.wizard.deployMessage =
          this.t("notice.stillBuilding");
        return;
      }
      try {
        const envs = await this.apiGet(
          `/api/v1/projects/${this.wizard.projectId}/environments?branch=${encodeURIComponent(branch)}`,
        );
        const env = envs[0];
        if (env && env.state === "running") {
          this.wizard.envId = env.id;
          this.wizard.deployState = "running";
          this.wizard.deploying = false;
          return;
        }
        if (env && env.state !== "building") {
          this.wizard.deploying = false;
          this.wizard.deployState = "failed";
          this.wizard.deployMessage = this.t("notice.envState", { state: this.stateLabel(env.state) });
          return;
        }
      } catch {
        // transient — keep polling
      }
      setTimeout(() => this.pollFirstDeploy(branch, attempt + 1), 2000);
    },

    webhookUrl() {
      return `${location.origin}/api/v1/webhooks/${this.wizard.provider}`;
    },

    async loadWebhookSecret() {
      this.wizard.webhookSecret = null;
      this.wizard.webhookSecretMissing = false;
      try {
        const res = await fetch(`${this.apiBase}/api/v1/setup/webhook-secret`, {
          headers: this.authHeaders(),
          cache: "no-store",
        });
        if (res.ok) {
          const body = await res.json();
          this.wizard.webhookSecret = body.webhook_secret ?? null;
          this.wizard.webhookSecretMissing = !body.webhook_secret;
        } else {
          this.wizard.webhookSecretMissing = true;
        }
      } catch {
        this.wizard.webhookSecretMissing = true;
      }
    },

    cliSnippet() {
      const tokenPart = this.token || "<your-token>";
      return `oxid context add prod --api ${location.origin} --token ${tokenPart}`;
    },

    curlSnippet() {
      return `curl -X POST ${location.origin}/api/v1/projects -H "Authorization: Bearer $OXID_TOKEN" -H "Content-Type: application/json" -d '{"repo_url":"https://github.com/you/app.git"}'`;
    },

    async copyText(text, label) {
      try {
        await navigator.clipboard.writeText(text);
        this.wizard.copied = label;
        setTimeout(() => {
          if (this.wizard.copied === label) {
            this.wizard.copied = "";
          }
        }, 1500);
      } catch {
        this.showNotice(this.t("notice.clipboard"));
      }
    },

    // ------------------------------------------------------------------
    // actions
    // ------------------------------------------------------------------

    async pause(env) {
      await this.apiSend("POST", `/api/v1/environments/${env.id}/pause`);
      this.showNotice(this.t("notice.paused", { branch: env.branch.name }));
      this.loadForRoute();
    },

    async wake(env) {
      await this.apiSend("POST", `/api/v1/environments/${env.id}/wake`);
      this.showNotice(this.t("notice.woken", { branch: env.branch.name }));
      this.loadForRoute();
    },

    async destroy(env) {
      if (
        !(await this.confirmDialog(
          this.t("confirm.destroyEnv", { branch: env.branch.name }),
        ))
      ) {
        return;
      }
      await this.apiSend("DELETE", `/api/v1/environments/${env.id}`);
      this.showNotice(this.t("notice.destroyed", { branch: env.branch.name }));
      if (this.route.name === "environment") {
        this.go("/ui/environments");
      } else {
        this.loadForRoute();
      }
    },

    async removeProject(project) {
      if (
        !(await this.confirmDialog(
          this.t("confirm.deleteProject", { name: project.name }),
        ))
      ) {
        return;
      }
      await this.apiSend("DELETE", `/api/v1/projects/${project.id}`);
      this.showNotice(this.t("notice.projectDeleted", { name: project.name }));
      this.go("/ui/projects");
    },

    // Seeds the settings form from the project's current values exactly
    // once per project (guarded by id, not object identity) — the 5s
    // auto-refresh replaces `this.projects` with fresh objects constantly,
    // which would otherwise stomp on whatever the user is mid-typing.
    // Wired via `x-effect` on the project page, so it re-fires on every
    // refresh but only actually resets the form the first time.
    maybeInitProjectSettingsForm(project) {
      if (!project || this._settingsFormProjectId === project.id) {
        return;
      }
      this._settingsFormProjectId = project.id;
      this.projectSettingsForm = {
        pause_after: project.config.pause_after,
        destroy_after: project.config.destroy_after,
        // Write-only, like project secrets — never echoed back by the API,
        // so this always starts blank regardless of whether one is set.
        git_token: "",
      };
    },

    async saveProjectSettings(project) {
      try {
        const res = await this.apiSend("PATCH", `/api/v1/projects/${project.id}`, {
          pause_after: this.projectSettingsForm.pause_after || null,
          destroy_after: this.projectSettingsForm.destroy_after || null,
        });
        const updated = await res.json();
        this.showNotice(
          this.t("notice.settingsUpdated", {
            name: updated.name,
            pause: updated.config.pause_after,
            destroy: updated.config.destroy_after,
          }),
        );
        // Force the next render to re-seed from the just-saved values.
        this._settingsFormProjectId = null;
        this.loadForRoute();
      } catch (err) {
        this.showNotice(this.t("notice.settingsFailed", { error: err.message }));
      }
    },

    async saveGitToken(project) {
      const token = this.projectSettingsForm.git_token.trim();
      if (!token) {
        this.showNotice(this.t("notice.enterToken"));
        return;
      }
      try {
        await this.apiSend("PATCH", `/api/v1/projects/${project.id}`, { git_token: token });
        this.projectSettingsForm.git_token = "";
        this.showNotice(this.t("notice.gitTokenSaved", { name: project.name }));
      } catch (err) {
        this.showNotice(this.t("notice.gitTokenFailed", { error: err.message }));
      }
    },

    async clearGitToken(project) {
      const confirmed = await this.confirmDialog(this.t("confirm.clearGitToken", { name: project.name }));
      if (!confirmed) {
        return;
      }
      try {
        await this.apiSend("PATCH", `/api/v1/projects/${project.id}`, { git_token: "" });
        this.projectSettingsForm.git_token = "";
        this.showNotice(this.t("notice.gitTokenCleared", { name: project.name }));
      } catch (err) {
        this.showNotice(this.t("notice.gitTokenClearFailed", { error: err.message }));
      }
    },

    async deployNew(project) {
      const branch = this.deployBranch.trim();
      if (!branch) {
        return;
      }
      try {
        const res = await this.apiSend("POST", `/api/v1/projects/${project.id}/deploy`, {
          branch,
        });
        const body = await res.json();
        this.showNotice(
          body.status === "queued"
            ? this.t("notice.deployQueued", { branch, position: body.position })
            : this.t("notice.deployStarted", { branch }),
        );
        this.deployBranch = "";
      } catch (err) {
        this.showNotice(this.t("notice.deployFailed", { error: err.message }));
      }
      this.loadForRoute();
    },

    async rollback(project, env) {
      if (
        !(await this.confirmDialog(
          this.t("confirm.rollback", { branch: env.branch.name }),
        ))
      ) {
        return;
      }
      try {
        await this.apiSend("POST", `/api/v1/projects/${project.id}/rollback`, {
          branch: env.branch.name,
        });
        this.showNotice(this.t("notice.rolledBack", { branch: env.branch.name }));
      } catch (err) {
        this.showNotice(this.t("notice.rollbackFailed", { error: err.message }));
      }
      this.loadForRoute();
    },

    // ------------------------------------------------------------------
    // secrets (global `/ui/secrets` or project-scoped `/ui/projects/:id/secrets`)
    // ------------------------------------------------------------------

    secretsTargetProjectId() {
      return this.route.name === "projectSecrets" ? this.route.params.id : null;
    },

    async reloadSecrets() {
      const projectId = this.secretsTargetProjectId();
      this.secretForm = {
        name: "",
        scope: projectId ? "project" : "global",
        value: "",
        branch: "",
      };
      const path = projectId ? `/api/v1/projects/${projectId}/secrets` : "/api/v1/secrets";
      const body = await this.apiGetQuiet(path);
      this.secretsList = body?.secrets ?? [];
    },

    async submitSecret() {
      const name = this.secretForm.name.trim();
      if (!name || !this.secretForm.value) {
        this.showNotice(this.t("notice.secretRequired"));
        return;
      }
      const projectId = this.secretsTargetProjectId();
      const path = projectId ? `/api/v1/projects/${projectId}/secrets` : "/api/v1/secrets";
      try {
        await this.apiSend("POST", path, {
          name,
          scope: projectId ? this.secretForm.scope : "global",
          value: this.secretForm.value,
          branch: this.secretForm.scope === "branch" ? this.secretForm.branch : null,
        });
        this.showNotice(this.t("notice.secretSet", { name }));
        await this.reloadSecrets();
      } catch (err) {
        this.showNotice(this.t("notice.secretFailed", { error: err.message }));
      }
    },

    async deleteSecret(secret) {
      if (!(await this.confirmDialog(this.t("confirm.deleteSecret", { name: secret.name, scope: secret.scope })))) {
        return;
      }
      const projectId = this.secretsTargetProjectId();
      const base = projectId
        ? `/api/v1/projects/${projectId}/secrets/${encodeURIComponent(secret.name)}`
        : `/api/v1/secrets/${encodeURIComponent(secret.name)}`;
      const qs = secret.branch ? `?branch=${encodeURIComponent(secret.branch)}` : "";
      try {
        await this.apiSend("DELETE", base + qs);
        this.showNotice(this.t("notice.secretDeleted", { name: secret.name }));
        await this.reloadSecrets();
      } catch (err) {
        this.showNotice(this.t("notice.secretDeleteFailed", { error: err.message }));
      }
    },

    // ------------------------------------------------------------------
    // admin: tokens, backup, key rotation
    // ------------------------------------------------------------------

    async createToken() {
      const name = this.newTokenName.trim();
      if (!name) {
        return;
      }
      try {
        const res = await this.apiSend("POST", "/api/v1/tokens", { name });
        const body = await res.json();
        this.showNotice(
          this.t("notice.tokenCreated", { name, token: body.token }),
        );
        this.newTokenName = "";
        this.loadForRoute();
      } catch (err) {
        this.showNotice(this.t("notice.tokenCreateFailed", { error: err.message }));
      }
    },

    async revokeToken(tok) {
      if (!(await this.confirmDialog(this.t("confirm.revokeToken", { name: tok.name })))) {
        return;
      }
      await this.apiSend("DELETE", `/api/v1/tokens/${tok.id}`);
      this.showNotice(this.t("notice.tokenRevoked", { name: tok.name }));
      this.loadForRoute();
    },

    async rotateKey() {
      if (
        !(await this.confirmDialog(
          this.t("confirm.rotateKey"),
        ))
      ) {
        return;
      }
      try {
        const res = await this.apiSend("POST", "/api/v1/rotate-key");
        const body = await res.json();
        this.showNotice(body.note ?? this.t("notice.keyRotated"));
      } catch (err) {
        this.showNotice(this.t("notice.keyRotationFailed", { error: err.message }));
      }
    },

    async downloadBackup() {
      try {
        const res = await this.apiSend("GET", "/api/v1/backup");
        const blob = await res.blob();
        const url = URL.createObjectURL(blob);
        const a = document.createElement("a");
        a.href = url;
        a.download = "oxid-backup.tar";
        a.click();
        URL.revokeObjectURL(url);
        this.showNotice(this.t("notice.backupDownloaded"));
      } catch (err) {
        this.showNotice(this.t("notice.backupFailed", { error: err.message }));
      }
    },

    // ------------------------------------------------------------------
    // live log stream — real SSE parsed over `fetch()`+`ReadableStream`
    // instead of the browser's native `EventSource`, which can't attach the
    // `Authorization` header this API requires.
    // ------------------------------------------------------------------

    async openLogStream(envId) {
      this.logLines = ["[loading log stream...]"];
      const controller = new AbortController();
      this._logAbort = controller;
      try {
        const res = await fetch(`${this.apiBase}/api/v1/environments/${envId}/logs/stream`, {
          headers: this.authHeaders(),
          signal: controller.signal,
          cache: "no-store",
        });
        if (!res.ok || !res.body) {
          this.logLines = [`[error opening stream: HTTP ${res.status}]`];
          return;
        }
        this.logLines = [];
        const reader = res.body.getReader();
        const decoder = new TextDecoder();
        let buffer = "";
        while (this.query.tab === "logs") {
          const { value, done } = await reader.read();
          if (done) {
            break;
          }
          buffer += decoder.decode(value, { stream: true });
          const frames = buffer.split("\n\n");
          buffer = frames.pop() ?? "";
          for (const frame of frames) {
            for (const line of frame.split("\n")) {
              if (line.startsWith("data:")) {
                this.logLines.push(line.slice(5).trimStart());
              }
            }
          }
          this.$nextTick(() => {
            const el = this.$refs.logOutput;
            if (el) {
              el.scrollTop = el.scrollHeight;
            }
          });
        }
      } catch (err) {
        if (err.name !== "AbortError") {
          this.logLines.push(`[stream error: ${err.message}]`);
        }
      }
    },

    closeLogStream() {
      if (this._logAbort) {
        this._logAbort.abort();
        this._logAbort = null;
      }
    },
  };
}
