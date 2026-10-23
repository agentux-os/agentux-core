//! Tool permission policies (ADR 0021): what agents may do without asking,
//! per ACP tool category, for the project and per role.
//!
//! ```yaml
//! permissions:
//!   default: ask
//!   read: allow
//!   execute: ask
//!   roles:
//!     reviewer:
//!       edit: deny
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// What happens to a tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    /// Runs without asking.
    Allow,
    /// Waits for the human (the inbox).
    Ask,
    /// The agent is told no; the run goes on.
    Deny,
}

impl Access {
    pub fn as_str(self) -> &'static str {
        match self {
            Access::Allow => "allow",
            Access::Ask => "ask",
            Access::Deny => "deny",
        }
    }
}

/// ACP's tool call categories: what a rule can name.
pub const CATEGORIES: [&str; 9] = [
    "read", "edit", "delete", "move", "search", "execute", "fetch", "think", "other",
];

/// One level of rules: the project's, or a role's.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Rules {
    /// For the categories this level does not name.
    pub default: Option<Access>,
    pub read: Option<Access>,
    pub edit: Option<Access>,
    pub delete: Option<Access>,
    #[serde(rename = "move")]
    pub move_: Option<Access>,
    pub search: Option<Access>,
    pub execute: Option<Access>,
    pub fetch: Option<Access>,
    pub think: Option<Access>,
    pub other: Option<Access>,
}

impl Rules {
    /// The rule for `category` at this level, if it names one.
    pub fn get(&self, category: &str) -> Option<Access> {
        match category {
            "read" => self.read,
            "edit" => self.edit,
            "delete" => self.delete,
            "move" => self.move_,
            "search" => self.search,
            "execute" => self.execute,
            "fetch" => self.fetch,
            "think" => self.think,
            "other" => self.other,
            _ => None,
        }
    }
}

/// The `permissions` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Permissions {
    pub default: Option<Access>,
    pub read: Option<Access>,
    pub edit: Option<Access>,
    pub delete: Option<Access>,
    #[serde(rename = "move")]
    pub move_: Option<Access>,
    pub search: Option<Access>,
    pub execute: Option<Access>,
    pub fetch: Option<Access>,
    pub think: Option<Access>,
    pub other: Option<Access>,
    /// Role name to its own rules, which come before the project's.
    pub roles: BTreeMap<String, Rules>,
}

/// A decision and the rule that made it, for the run's log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decided {
    pub access: Access,
    /// `permissions.roles.reviewer.edit`, `permissions.default`, or
    /// `no permissions rule` when nothing in the file applies.
    pub rule: String,
}

impl Permissions {
    fn project(&self) -> Rules {
        Rules {
            default: self.default,
            read: self.read,
            edit: self.edit,
            delete: self.delete,
            move_: self.move_,
            search: self.search,
            execute: self.execute,
            fetch: self.fetch,
            think: self.think,
            other: self.other,
        }
    }

    /// What a tool call of `category` by `role` gets: the role's rule, the
    /// project's, the role's default, the project's default, else `ask`.
    /// A category ACP did not name counts as `other`.
    pub fn decide(&self, role: &str, category: &str) -> Decided {
        let category = if CATEGORIES.contains(&category) {
            category
        } else {
            "other"
        };
        let role_rules = self.roles.get(role);
        let project = self.project();
        let found = [
            (
                role_rules.and_then(|r| r.get(category)),
                format!("permissions.roles.{role}.{category}"),
            ),
            (project.get(category), format!("permissions.{category}")),
            (
                role_rules.and_then(|r| r.default),
                format!("permissions.roles.{role}.default"),
            ),
            (self.default, "permissions.default".to_string()),
        ]
        .into_iter()
        .find_map(|(access, rule)| access.map(|access| Decided { access, rule }));
        found.unwrap_or(Decided {
            access: Access::Ask,
            rule: "no permissions rule".into(),
        })
    }

    /// Whether the section says anything at all.
    pub fn is_empty(&self) -> bool {
        *self == Permissions::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(yaml: &str) -> Permissions {
        serde_saphyr::from_str(yaml).unwrap()
    }

    #[test]
    fn the_role_comes_first_then_the_project_then_the_defaults() {
        let p = policy(
            "default: deny\nread: allow\nexecute: ask\nroles:\n  reviewer:\n    edit: deny\n    default: ask\n",
        );
        let d = |role, category| {
            let d = p.decide(role, category);
            (d.access, d.rule)
        };
        assert_eq!(
            d("reviewer", "edit"),
            (Access::Deny, "permissions.roles.reviewer.edit".into())
        );
        assert_eq!(
            d("reviewer", "read"),
            (Access::Allow, "permissions.read".into())
        );
        assert_eq!(
            d("reviewer", "fetch"),
            (Access::Ask, "permissions.roles.reviewer.default".into())
        );
        assert_eq!(
            d("implementer", "fetch"),
            (Access::Deny, "permissions.default".into())
        );
        assert_eq!(
            d("implementer", "execute"),
            (Access::Ask, "permissions.execute".into())
        );
        assert_eq!(
            d("implementer", "teleport"),
            (Access::Deny, "permissions.default".into()),
            "unknown: other"
        );
    }

    #[test]
    fn without_a_policy_everything_is_asked() {
        let p = Permissions::default();
        assert!(p.is_empty());
        assert_eq!(p.decide("any", "execute").access, Access::Ask);
        assert_eq!(p.decide("any", "execute").rule, "no permissions rule");
    }

    #[test]
    fn unknown_categories_and_decisions_are_refused() {
        assert!(serde_saphyr::from_str::<Permissions>("teleport: allow\n").is_err());
        assert!(serde_saphyr::from_str::<Permissions>("read: maybe\n").is_err());
        assert!(serde_saphyr::from_str::<Permissions>("roles:\n  r:\n    write: allow\n").is_err());
        assert_eq!(
            policy("move: allow\n").decide("r", "move").access,
            Access::Allow
        );
    }
}
