//! Who may do what, for how long, on which projects.
//!
//! Access used to be one bit — a credential was either the master token or a
//! named one, and a named one could optionally be restricted to some
//! projects. Inside a project it could do *everything*: read logs, deploy,
//! rewrite secrets, change the branch filter, delete the project. That is
//! the right amount of power for the person running the server and far too
//! much for the developer who just wants to watch their branch come up.
//!
//! So a grant now carries three things a devops actually needs to say:
//!
//! - **what** — a [`Role`], from `viewer` to `admin`;
//! - **where** — the projects it applies to, or every project;
//! - **until when** — an optional expiry, because the contractor who needed
//!   access for a sprint should not still have it next year, and revoking by
//!   hand is a thing everyone means to do and nobody does.
//!
//! Two properties keep this honest rather than decorative:
//!
//! - **Roles are ordered and cumulative.** Each one can do everything the
//!   one below it can, and a test pins that. A permission matrix where a
//!   middle role is missing something a lower one has is a bug nobody finds
//!   by reading it.
//! - **Denial says why, and the caller decides what to reveal.** Being out
//!   of scope must be indistinguishable from the project not existing (a
//!   `404`), while a role or an expiry is worth explaining (`403`) — a
//!   developer told "your access expired" opens a ticket; one told "404"
//!   files a bug against the daemon.

use serde::{Deserialize, Serialize};

/// What a credential is allowed to do, in increasing order of power.
///
/// Deliberately four. Every extra role is one an operator has to hold in
/// their head at the moment they are granting access to a colleague, and
/// the ones below map onto how teams already talk about this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Read-only. Sees projects, environments, logs and the audit trail;
    /// changes nothing. The role for a product owner watching previews, or
    /// for a dashboard on a wall.
    Viewer,
    /// The day-to-day developer: everything a viewer can, plus deploying,
    /// rolling back, pausing, waking and destroying *environments*. Cannot
    /// read or write secrets, and cannot change the project itself.
    Developer,
    /// Owns the projects in its scope: everything a developer can, plus
    /// secrets, the project's settings and its branch filter, and deleting
    /// the project. Still cannot touch the node.
    Maintainer,
    /// The server's operator: everything, including node-wide operations —
    /// infra, backups, key rotation, and issuing access to everyone else.
    Admin,
}

/// A single thing a request wants to do.
///
/// Named for the action rather than the route, so a new endpoint reuses an
/// existing capability instead of inventing a rule nobody reviewed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// See a project, its environments, its logs and its history.
    Read,
    /// Deploy, roll back, pause, wake, or destroy an environment.
    Operate,
    /// Read or write secrets and environment variables.
    Secrets,
    /// Change a project's settings, or delete the project.
    ManageProject,
    /// Register a new project on the node.
    CreateProject,
    /// Node-wide: stats, infra, the deploy queue, backups, key rotation.
    ManageNode,
    /// Issue, list and revoke other people's access.
    ManageAccess,
}

impl Role {
    /// Whether this role permits `capability`.
    ///
    /// Written as the *lowest role that unlocks each capability* rather than
    /// a per-role list, which is what makes the ordering property below true
    /// by construction instead of by review.
    #[must_use]
    pub fn allows(self, capability: Capability) -> bool {
        self >= Self::minimum_for(capability)
    }

    /// The least powerful role that may do `capability`.
    #[must_use]
    pub fn minimum_for(capability: Capability) -> Self {
        match capability {
            Capability::Read => Self::Viewer,
            Capability::Operate => Self::Developer,
            // Secrets are the line between "can deploy the app" and "can
            // read the production database password the app is given".
            Capability::Secrets | Capability::ManageProject => Self::Maintainer,
            Capability::CreateProject | Capability::ManageNode | Capability::ManageAccess => {
                Self::Admin
            }
        }
    }

    /// The role's wire and CLI spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Developer => "developer",
            Self::Maintainer => "maintainer",
            Self::Admin => "admin",
        }
    }

    /// Every role, weakest first — for `--help` text and the dashboard's
    /// role picker, so neither can drift from this enum.
    #[must_use]
    pub fn all() -> [Self; 4] {
        [Self::Viewer, Self::Developer, Self::Maintainer, Self::Admin]
    }
}

impl std::str::FromStr for Role {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::all()
            .into_iter()
            .find(|r| r.as_str() == raw.trim().to_ascii_lowercase())
            .ok_or_else(|| {
                let names: Vec<_> = Role::all().iter().map(|r| r.as_str()).collect();
                format!(
                    "unknown role `{raw}` — expected one of {}",
                    names.join(", ")
                )
            })
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What one credential is allowed to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    /// What this credential may do within its scope.
    pub role: Role,
    /// Projects it is limited to. `None` is every project, present and
    /// future — the shape an admin or a node-wide CI credential wants.
    pub projects: Option<Vec<u64>>,
    /// Unix seconds after which the credential stops working. `None` never
    /// expires.
    pub expires_at: Option<i64>,
    /// Whether access is suspended. Distinct from revocation: a suspended
    /// credential can be switched back on, which is what an operator wants
    /// for someone on leave, while revoking is permanent.
    pub suspended: bool,
}

impl Grant {
    /// An unrestricted, non-expiring admin grant — the master credential.
    #[must_use]
    pub fn master() -> Self {
        Self {
            role: Role::Admin,
            projects: None,
            expires_at: None,
            suspended: false,
        }
    }

    /// Whether the grant is usable at all at `now` (unix seconds),
    /// regardless of what is being asked.
    #[must_use]
    pub fn is_active(&self, now: i64) -> bool {
        !self.suspended && self.expires_at.is_none_or(|at| now < at)
    }

    /// Whether this grant permits `capability`, on `project` when the action
    /// belongs to one.
    ///
    /// Checked in the order a person would explain it: is this credential
    /// alive, does it reach this project, does its role allow this. Scope
    /// comes before role deliberately — a developer asking about another
    /// team's project must be told the project does not exist, not that
    /// their role is too low, which would confirm it does.
    ///
    /// # Errors
    /// The [`Denial`] that stopped it.
    pub fn authorize(
        &self,
        capability: Capability,
        project: Option<u64>,
        now: i64,
    ) -> Result<(), Denial> {
        if self.suspended {
            return Err(Denial::Suspended);
        }
        if let Some(at) = self.expires_at
            && now >= at
        {
            return Err(Denial::Expired(at));
        }
        if let (Some(scopes), Some(project)) = (self.projects.as_ref(), project)
            && !scopes.contains(&project)
        {
            return Err(Denial::OutOfScope(project));
        }
        // A scoped credential can never act node-wide, whatever its role or
        // capability: "admin of project 3" is not an admin of the server.
        //
        // The condition is deliberately *only* "scoped, and this action
        // names no project" — an earlier version also asked whether the
        // capability looked project-local, which let a scoped credential
        // write a **global** secret, since `Secrets` is project-local in
        // every other context. A global secret is injected into every
        // project's deploys. Any action without a project is node-wide by
        // construction, and that is the whole test.
        if self.projects.is_some() && project.is_none() {
            return Err(Denial::NodeWide);
        }
        if !self.role.allows(capability) {
            return Err(Denial::RoleTooLow {
                have: self.role,
                need: Role::minimum_for(capability),
            });
        }
        Ok(())
    }
}

/// Why a request was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Denial {
    /// The credential is suspended.
    Suspended,
    /// The credential expired at this unix timestamp.
    Expired(i64),
    /// The credential is not scoped to this project. The caller must report
    /// this as "not found" — see [`Denial::hides_existence`].
    OutOfScope(u64),
    /// A project-scoped credential attempted a node-wide action.
    NodeWide,
    /// The role is below what the action needs.
    RoleTooLow {
        /// The role the credential has.
        have: Role,
        /// The role the action needs.
        need: Role,
    },
}

impl Denial {
    /// Whether answering honestly would leak that something exists.
    ///
    /// Only being out of scope does: everything else is about the caller's
    /// own credential, which they already know about, and explaining it
    /// saves them a support ticket.
    #[must_use]
    pub fn hides_existence(&self) -> bool {
        matches!(self, Self::OutOfScope(_))
    }

    /// A message for the person who hit it.
    ///
    /// Not translated: it is also what lands in the audit trail and in logs,
    /// which are searched by their text.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Suspended => {
                "this access has been suspended — ask an operator to re-enable it".to_owned()
            }
            Self::Expired(at) => {
                format!("this access expired (unix {at}) — ask an operator to issue a new one")
            }
            Self::OutOfScope(project) => format!("project `{project}` is out of scope"),
            Self::NodeWide => {
                "this action affects the whole node and needs an unscoped credential".to_owned()
            }
            Self::RoleTooLow { have, need } => {
                format!("this action needs the `{need}` role or higher; this access is `{have}`")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000;

    fn grant(role: Role) -> Grant {
        Grant {
            role,
            projects: None,
            expires_at: None,
            suspended: false,
        }
    }

    #[test]
    fn roles_are_cumulative() {
        // The property that makes the matrix reviewable: anything a weaker
        // role may do, every stronger one may do too. A hole here is the
        // kind nobody spots by reading a per-role list.
        let caps = [
            Capability::Read,
            Capability::Operate,
            Capability::Secrets,
            Capability::ManageProject,
            Capability::CreateProject,
            Capability::ManageNode,
            Capability::ManageAccess,
        ];
        for cap in caps {
            for (weaker, stronger) in Role::all().iter().zip(Role::all().iter().skip(1)) {
                assert!(
                    !weaker.allows(cap) || stronger.allows(cap),
                    "{stronger} cannot {cap:?} but the weaker {weaker} can"
                );
            }
        }
    }

    #[test]
    fn a_viewer_reads_and_changes_nothing() {
        let g = grant(Role::Viewer);
        assert!(g.authorize(Capability::Read, Some(1), NOW).is_ok());
        assert_eq!(
            g.authorize(Capability::Operate, Some(1), NOW),
            Err(Denial::RoleTooLow {
                have: Role::Viewer,
                need: Role::Developer
            })
        );
    }

    #[test]
    fn a_developer_deploys_but_never_reads_secrets() {
        // The line that matters most: deploying the app and reading the
        // credentials the app is handed are different powers.
        let g = grant(Role::Developer);
        assert!(g.authorize(Capability::Operate, Some(1), NOW).is_ok());
        assert!(g.authorize(Capability::Secrets, Some(1), NOW).is_err());
        assert!(
            g.authorize(Capability::ManageProject, Some(1), NOW)
                .is_err()
        );
    }

    #[test]
    fn a_maintainer_owns_its_projects_but_not_the_node() {
        let g = grant(Role::Maintainer);
        assert!(g.authorize(Capability::Secrets, Some(1), NOW).is_ok());
        assert!(g.authorize(Capability::ManageProject, Some(1), NOW).is_ok());
        assert!(g.authorize(Capability::ManageNode, None, NOW).is_err());
        assert!(g.authorize(Capability::ManageAccess, None, NOW).is_err());
    }

    #[test]
    fn scope_is_checked_before_role_so_it_cannot_confirm_a_project_exists() {
        // A viewer scoped to project 1 asking to *deploy* project 2 must be
        // told 2 is out of scope, not that their role is too low — the
        // latter would confirm project 2 exists.
        let g = Grant {
            role: Role::Viewer,
            projects: Some(vec![1]),
            expires_at: None,
            suspended: false,
        };
        assert_eq!(
            g.authorize(Capability::Operate, Some(2), NOW),
            Err(Denial::OutOfScope(2))
        );
        assert!(
            g.authorize(Capability::Operate, Some(2), NOW)
                .unwrap_err()
                .hides_existence()
        );
    }

    /// Regression: a scoped credential could write a *global* secret,
    /// because `Secrets` is a project-local capability everywhere else. A
    /// global secret is injected into every project's deploys, so scope has
    /// to win on any action that names no project, whatever the capability.
    #[test]
    fn a_scoped_credential_cannot_write_a_global_secret() {
        let g = Grant {
            role: Role::Maintainer,
            projects: Some(vec![1]),
            expires_at: None,
            suspended: false,
        };
        assert!(g.authorize(Capability::Secrets, Some(1), NOW).is_ok());
        assert_eq!(
            g.authorize(Capability::Secrets, None, NOW),
            Err(Denial::NodeWide)
        );
    }

    #[test]
    fn an_admin_of_one_project_is_not_an_admin_of_the_server() {
        let g = Grant {
            role: Role::Admin,
            projects: Some(vec![1]),
            expires_at: None,
            suspended: false,
        };
        assert!(g.authorize(Capability::ManageProject, Some(1), NOW).is_ok());
        assert_eq!(
            g.authorize(Capability::ManageNode, None, NOW),
            Err(Denial::NodeWide)
        );
        assert_eq!(
            g.authorize(Capability::ManageAccess, None, NOW),
            Err(Denial::NodeWide)
        );
    }

    #[test]
    fn expiry_stops_a_credential_without_anyone_remembering_to() {
        let g = Grant {
            role: Role::Admin,
            projects: None,
            expires_at: Some(NOW),
            suspended: false,
        };
        // Exactly at the expiry it is already gone: an inclusive boundary
        // would leave a credential valid for the whole second it dies.
        assert_eq!(
            g.authorize(Capability::Read, None, NOW),
            Err(Denial::Expired(NOW))
        );
        assert!(g.authorize(Capability::Read, None, NOW - 1).is_ok());
        assert!(!g.is_active(NOW));
        assert!(g.is_active(NOW - 1));
    }

    #[test]
    fn suspension_is_reported_before_anything_else() {
        // Someone whose access was switched off should be told that, not
        // that their role is too low for what they tried.
        let g = Grant {
            role: Role::Viewer,
            projects: Some(vec![1]),
            expires_at: None,
            suspended: true,
        };
        assert_eq!(
            g.authorize(Capability::ManageNode, Some(9), NOW),
            Err(Denial::Suspended)
        );
    }

    #[test]
    fn the_master_grant_can_do_everything() {
        let g = Grant::master();
        for cap in [
            Capability::Read,
            Capability::Operate,
            Capability::Secrets,
            Capability::ManageProject,
            Capability::CreateProject,
            Capability::ManageNode,
            Capability::ManageAccess,
        ] {
            assert!(g.authorize(cap, None, NOW).is_ok(), "{cap:?}");
        }
    }

    #[test]
    fn roles_round_trip_through_their_wire_name() {
        for role in Role::all() {
            assert_eq!(role.as_str().parse::<Role>().unwrap(), role);
        }
        assert!("root".parse::<Role>().is_err());
        // The error names the valid options rather than only the bad one.
        assert!("root".parse::<Role>().unwrap_err().contains("maintainer"));
    }

    #[test]
    fn only_being_out_of_scope_hides_existence() {
        assert!(Denial::OutOfScope(1).hides_existence());
        for d in [
            Denial::Suspended,
            Denial::Expired(1),
            Denial::NodeWide,
            Denial::RoleTooLow {
                have: Role::Viewer,
                need: Role::Admin,
            },
        ] {
            assert!(!d.hides_existence(), "{d:?}");
            assert!(!d.describe().is_empty());
        }
    }
}
