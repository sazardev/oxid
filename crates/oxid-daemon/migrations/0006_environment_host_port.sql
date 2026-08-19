-- Resource admission control (task #38) fixed the host-side capacity check,
-- but direct-port-publish mode (no Traefik) still bound every environment to
-- its project's single configured `[routing].port` — a live branch already
-- holding it made any other branch's deploy fail outright instead of Oxid
-- just finding another free port itself. `run()` now always asks Docker to
-- pick the published host port; this column stores which one it actually
-- got, so the dashboard/CLI can show a real, reachable address per
-- environment instead of the (now potentially wrong) project-wide port.
ALTER TABLE environments ADD COLUMN host_port INTEGER;
