// Oxid dashboard logic. No build step, no framework beyond Alpine.js
// (vendored in vendor/alpine.min.js) — this file is loaded as a plain
// classic script and defines the `dashboard()` factory Alpine looks up
// from `x-data="dashboard()"` in index.html.
function dashboard() {
  return {
    apiBase: "",
    token: "",
    online: false,
    authError: false,
    connError: false,
    projects: [],
    stats: {},
    queue: [],
    audit: [],
    logsOpen: false,
    logsBranch: "",
    logLines: [],
    refreshIntervalSecs: 5,
    _logAbort: null,
    _timer: null,

    init() {
      this.token = localStorage.getItem("oxid_token") || "";
      this.refreshAll();
      this._timer = setInterval(() => this.refreshAll(), this.refreshIntervalSecs * 1000);
    },

    saveToken() {
      localStorage.setItem("oxid_token", this.token);
      this.refreshAll();
    },

    authHeaders() {
      const headers = {};
      if (this.token) {
        headers.Authorization = `Bearer ${this.token}`;
      }
      return headers;
    },

    async apiGet(path) {
      const res = await fetch(this.apiBase + path, { headers: this.authHeaders() });
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

    async apiSend(method, path) {
      const res = await fetch(this.apiBase + path, { method, headers: this.authHeaders() });
      if (res.status === 401 || res.status === 403) {
        this.authError = true;
        throw new Error("unauthorized");
      }
      this.authError = false;
      if (!res.ok) {
        throw new Error(`${res.status}`);
      }
      return res;
    },

    async refreshAll() {
      try {
        const [stats, projects, queue, audit] = await Promise.all([
          this.apiGet("/api/v1/stats"),
          this.loadProjectsWithEnvironments(),
          this.apiGet("/api/v1/queue"),
          this.apiGet("/api/v1/audit?limit=30"),
        ]);
        this.stats = stats;
        this.projects = projects;
        this.queue = queue;
        this.audit = audit;
        this.online = true;
        this.connError = false;
      } catch (err) {
        this.online = false;
        if (err.message !== "unauthorized") {
          this.connError = true;
        }
      }
    },

    async loadProjectsWithEnvironments() {
      const projects = await this.apiGet("/api/v1/projects");
      for (const project of projects) {
        const envs = await this.apiGet(`/api/v1/projects/${project.id}/environments`);
        project.environments = this.latestPerBranch(envs);
      }
      return projects;
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

    async pause(env) {
      await this.apiSend("POST", `/api/v1/environments/${env.id}/pause`);
      this.refreshAll();
    },

    async wake(env) {
      await this.apiSend("POST", `/api/v1/environments/${env.id}/wake`);
      this.refreshAll();
    },

    async destroy(env) {
      if (!confirm(`Destroy environment \`${env.branch.name}\`? This cannot be undone.`)) {
        return;
      }
      await this.apiSend("DELETE", `/api/v1/environments/${env.id}`);
      this.refreshAll();
    },

    async openLogs(env) {
      this.logsOpen = true;
      this.logsBranch = env.branch.name;
      this.logLines = ["[loading log stream...]"];
      if (this._logAbort) {
        this._logAbort.abort();
      }
      const controller = new AbortController();
      this._logAbort = controller;
      try {
        const res = await fetch(
          `${this.apiBase}/api/v1/environments/${env.id}/logs/stream`,
          { headers: this.authHeaders(), signal: controller.signal },
        );
        if (!res.ok || !res.body) {
          this.logLines = [`[error opening stream: HTTP ${res.status}]`];
          return;
        }
        this.logLines = [];
        const reader = res.body.getReader();
        const decoder = new TextDecoder();
        let buffer = "";
        while (this.logsOpen) {
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

    closeLogs() {
      this.logsOpen = false;
      if (this._logAbort) {
        this._logAbort.abort();
        this._logAbort = null;
      }
    },
  };
}
