-- Zero-downtime redeploys: a redeploy of an already-live branch used to
-- destroy the old container BEFORE building/starting the new one, so there
-- was always a gap where the branch was unreachable (worse still, in
-- direct-publish mode the address itself changed every redeploy, so even a
-- perfectly-timed swap would still break anyone already using the old
-- port). Now the new instance is built and started fully, health-checked,
-- and only then does the old one get torn down.
--
-- `public_port` is the branch's new stable address in direct-publish mode:
-- bound once by Oxid's own built-in reverse proxy and reused across every
-- redeploy, with the proxy's upstream target swapped atomically to the new
-- container once it's confirmed ready (see `service/proxy.rs`).
--
-- `container_name` is persisted per-deployment so a redeploy's new instance
-- can have a name distinct from the still-running old one (both briefly
-- coexist during the swap) instead of the old fixed `oxid-{project}-{branch}`
-- scheme, which assumed only one instance ever existed at a time.
ALTER TABLE environments ADD COLUMN public_port INTEGER;
ALTER TABLE environments ADD COLUMN container_name TEXT;
