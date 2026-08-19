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
    confirmModal: { open: false, message: "", resolve: null },
    _logAbort: null,
    _timer: null,

    init() {
      this.token = localStorage.getItem("oxid_token") || "";
      window.addEventListener("popstate", () => this.onRouteChange());
      this.onRouteChange();
      this._timer = setInterval(() => this.refreshCurrentPage(), this.refreshIntervalSecs * 1000);
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
          this.showNotice(`Failed to load page data: ${err.message}`);
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
    // address is the project's own `[routing].port` published on whatever
    // host is running the daemon (the same host this dashboard is being
    // viewed from).
    envLink(env, projectId) {
      if (this.stats.traefik_enabled) {
        return `http://${env.url}`;
      }
      const port = this.projects.find((p) => p.id === (projectId ?? env.projectId))?.config
        ?.port;
      return port ? `${location.protocol}//${location.hostname}:${port}/` : `http://${env.url}`;
    },

    envLinkLabel(env, projectId) {
      if (this.stats.traefik_enabled) {
        return env.url;
      }
      const port = this.projects.find((p) => p.id === (projectId ?? env.projectId))?.config
        ?.port;
      return port ? `${location.hostname}:${port}` : env.url;
    },

    hostMemoryLabel() {
      if (!this.stats.host_total_memory_bytes) {
        return "—";
      }
      const gb = this.stats.host_total_memory_bytes / 1_073_741_824;
      return `${gb.toFixed(1)}G / ${this.stats.host_cpu_count ?? "?"} cpu`;
    },

    formatTime(value) {
      if (!Array.isArray(value)) {
        return String(value ?? "");
      }
      const [year, day, hour, min, sec] = value;
      const pad = (n, w) => String(n ?? 0).padStart(w, "0");
      return `${pad(year, 4)}-day${pad(day, 3)} ${pad(hour, 2)}:${pad(min, 2)}:${pad(sec, 2)}`;
    },

    // ------------------------------------------------------------------
    // actions
    // ------------------------------------------------------------------

    async pause(env) {
      await this.apiSend("POST", `/api/v1/environments/${env.id}/pause`);
      this.showNotice(`Paused \`${env.branch.name}\`.`);
      this.loadForRoute();
    },

    async wake(env) {
      await this.apiSend("POST", `/api/v1/environments/${env.id}/wake`);
      this.showNotice(`Woke \`${env.branch.name}\`.`);
      this.loadForRoute();
    },

    async destroy(env) {
      if (
        !(await this.confirmDialog(
          `Destroy environment \`${env.branch.name}\`? This cannot be undone.`,
        ))
      ) {
        return;
      }
      await this.apiSend("DELETE", `/api/v1/environments/${env.id}`);
      this.showNotice(`Destroyed \`${env.branch.name}\`.`);
      if (this.route.name === "environment") {
        this.go("/ui/environments");
      } else {
        this.loadForRoute();
      }
    },

    async removeProject(project) {
      if (
        !(await this.confirmDialog(
          `Permanently delete project \`${project.name}\`? This destroys every environment and all its secrets.`,
        ))
      ) {
        return;
      }
      await this.apiSend("DELETE", `/api/v1/projects/${project.id}`);
      this.showNotice(`Deleted project \`${project.name}\`.`);
      this.go("/ui/projects");
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
            ? `\`${branch}\` queued for capacity (position ${body.position}).`
            : `\`${branch}\` deployed.`,
        );
        this.deployBranch = "";
      } catch (err) {
        this.showNotice(`Deploy failed: ${err.message}`);
      }
      this.loadForRoute();
    },

    async rollback(project, env) {
      if (
        !(await this.confirmDialog(
          `Roll back \`${env.branch.name}\` to the deploy immediately before this one?`,
        ))
      ) {
        return;
      }
      try {
        await this.apiSend("POST", `/api/v1/projects/${project.id}/rollback`, {
          branch: env.branch.name,
        });
        this.showNotice(`Rolled back \`${env.branch.name}\`.`);
      } catch (err) {
        this.showNotice(`Rollback failed: ${err.message}`);
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
        this.showNotice("Secret name and value are both required.");
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
        this.showNotice(`Secret \`${name}\` set.`);
        await this.reloadSecrets();
      } catch (err) {
        this.showNotice(`Setting secret failed: ${err.message}`);
      }
    },

    async deleteSecret(secret) {
      if (!(await this.confirmDialog(`Delete secret \`${secret.name}\` (${secret.scope})?`))) {
        return;
      }
      const projectId = this.secretsTargetProjectId();
      const base = projectId
        ? `/api/v1/projects/${projectId}/secrets/${encodeURIComponent(secret.name)}`
        : `/api/v1/secrets/${encodeURIComponent(secret.name)}`;
      const qs = secret.branch ? `?branch=${encodeURIComponent(secret.branch)}` : "";
      try {
        await this.apiSend("DELETE", base + qs);
        this.showNotice(`Secret \`${secret.name}\` deleted.`);
        await this.reloadSecrets();
      } catch (err) {
        this.showNotice(`Deleting secret failed: ${err.message}`);
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
          `Token created for \`${name}\`: ${body.token} — copy it now, it won't be shown again.`,
        );
        this.newTokenName = "";
        this.loadForRoute();
      } catch (err) {
        this.showNotice(`Creating token failed: ${err.message}`);
      }
    },

    async revokeToken(tok) {
      if (!(await this.confirmDialog(`Revoke token \`${tok.name}\`?`))) {
        return;
      }
      await this.apiSend("DELETE", `/api/v1/tokens/${tok.id}`);
      this.showNotice(`Revoked token \`${tok.name}\`.`);
      this.loadForRoute();
    },

    async rotateKey() {
      if (
        !(await this.confirmDialog(
          "Rotate the master encryption key? Every secret is re-encrypted with zero downtime, but this cannot be undone.",
        ))
      ) {
        return;
      }
      try {
        const res = await this.apiSend("POST", "/api/v1/rotate-key");
        const body = await res.json();
        this.showNotice(body.note ?? "Master key rotated.");
      } catch (err) {
        this.showNotice(`Key rotation failed: ${err.message}`);
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
        this.showNotice("Backup downloaded.");
      } catch (err) {
        this.showNotice(`Backup failed: ${err.message}`);
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
