//! Who the host knows: its users, each a person with a role and the cadrs IDs they use (one for
//! each identity on each device, and each browser), and the maintainers of each document. They
//! are in `users.ron` in the host's data dir, which the host reads again as it scans, so a
//! change made while it runs (`cadrs --host-user`) counts within seconds.
//!
//! The host answers no one else: a cadrs ID that isn't one of its users' is refused whatever it
//! asks, link or no link. Who may open which document stays in the document's own sharing list,
//! as everywhere in cadrs; the roles say what someone may do with the host itself.

use std::collections::BTreeMap;
use std::path::Path;

use cadrs_core::DocumentId;
use cadrs_net::bevy_iroh::iroh::EndpointId;

/// The host's users, in its data dir.
pub const USERS_FILE: &str = "users.ron";

/// What a user may do with the host, least first.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum Role {
    /// Works on the host's documents that are shared with them; gives it none.
    Collaborator,
    /// Gives the host documents (**Transfer ownership**) and manages those they maintain.
    Maintainer,
    /// The host's settings and users, and every document.
    Admin,
}

impl Role {
    pub const ALL: [Role; 3] = [Role::Admin, Role::Maintainer, Role::Collaborator];

    pub fn label(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Maintainer => "maintainer",
            Role::Collaborator => "collaborator",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        Role::ALL
            .into_iter()
            .find(|r| r.label().eq_ignore_ascii_case(s.trim()))
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// A person.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct User {
    /// Unique on the host, whatever the case.
    pub name: String,
    pub role: Role,
    /// Their sign-in with the team (an address), for the web admin page and the web app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sign_in: Option<String>,
    /// The cadrs IDs they use.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ids: Vec<String>,
}

/// What `users.ron` holds.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Users {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub users: Vec<User>,
    /// The maintainers of each document, by name (maintainers and admins only).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub maintainers: BTreeMap<DocumentId, Vec<String>>,
}

impl Users {
    /// The users in `dir`: none when it has no `users.ron`.
    pub fn read(dir: &Path) -> Result<Users, String> {
        let path = dir.join(USERS_FILE);
        if !cadrs_fs::exists(&path) {
            return Ok(Users::default());
        }
        let text =
            cadrs_fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        ron::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Writes them to `dir` in one go (a host reading the file never sees half of it).
    pub fn write(&self, dir: &Path) -> Result<(), String> {
        self.check()?;
        let text = ron::ser::to_string_pretty(self, ron::ser::PrettyConfig::default())
            .map_err(|e| e.to_string())?;
        let path = dir.join(USERS_FILE);
        let tmp = path.with_extension("ron.tmp");
        cadrs_fs::write(&tmp, text)
            .and_then(|_| cadrs_fs::rename(&tmp, &path))
            .map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Reads the users in `dir`, changes them with `f` and writes them back: what `f` returns.
    pub fn edit<T>(
        dir: &Path,
        f: impl FnOnce(&mut Users) -> Result<T, String>,
    ) -> Result<(Users, T), String> {
        let mut users = Users::read(dir)?;
        let out = f(&mut users)?;
        users.write(dir)?;
        Ok((users, out))
    }

    /// Unique names, valid cadrs IDs each used by one person, maintainers who exist and may.
    pub fn check(&self) -> Result<(), String> {
        let mut ids: BTreeMap<&str, &str> = BTreeMap::new();
        for (i, u) in self.users.iter().enumerate() {
            if u.name.trim().is_empty() {
                return Err("A user needs a name".into());
            }
            if self.users[..i]
                .iter()
                .any(|o| o.name.eq_ignore_ascii_case(&u.name))
            {
                return Err(format!("Two users are called {}", u.name));
            }
            if let Some(s) = &u.sign_in
                && let Some(o) = self.users[..i].iter().find(|o| {
                    o.sign_in
                        .as_ref()
                        .is_some_and(|t| t.eq_ignore_ascii_case(s))
                })
            {
                return Err(format!("{} and {} both sign in as {s}", o.name, u.name));
            }
            for id in &u.ids {
                if id.parse::<EndpointId>().is_err() {
                    return Err(format!("{id} ({}) is not a cadrs ID", u.name));
                }
                if let Some(other) = ids.insert(id, &u.name) {
                    return Err(format!("{id} is both {other}'s and {}'s", u.name));
                }
            }
        }
        for names in self.maintainers.values() {
            for name in names {
                match self.by_name(name) {
                    None => return Err(format!("No user is called {name}")),
                    Some(u) if u.role < Role::Maintainer => {
                        return Err(format!(
                            "{} is a {}: only maintainers and admins maintain documents",
                            u.name, u.role
                        ));
                    }
                    Some(_) => {}
                }
            }
        }
        Ok(())
    }

    /// Who signs in (with the team's sign-in) as `sign_in`.
    pub fn by_sign_in(&self, sign_in: &str) -> Option<&User> {
        self.users.iter().find(|u| {
            u.sign_in
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case(sign_in.trim()))
        })
    }

    /// Whose cadrs ID `id` is.
    pub fn by_id(&self, id: &str) -> Option<&User> {
        self.users.iter().find(|u| u.ids.iter().any(|i| i == id))
    }

    pub fn by_name(&self, name: &str) -> Option<&User> {
        self.users
            .iter()
            .find(|u| u.name.eq_ignore_ascii_case(name.trim()))
    }

    /// What the owner of cadrs ID `id` may do with the host; `None` for a stranger.
    pub fn role(&self, id: &str) -> Option<Role> {
        self.by_id(id).map(|u| u.role)
    }

    /// Whether the owner of cadrs ID `id` manages `document`: an admin, or one of its
    /// maintainers.
    pub fn maintains(&self, id: &str, document: DocumentId) -> bool {
        self.by_id(id).is_some_and(|u| self.manages(u, document))
    }

    /// Whether `user` manages `document`: an admin, or one of its maintainers.
    pub fn manages(&self, user: &User, document: DocumentId) -> bool {
        user.role == Role::Admin
            || (user.role >= Role::Maintainer
                && self
                    .maintainers_of(document)
                    .iter()
                    .any(|n| n.eq_ignore_ascii_case(&user.name)))
    }

    /// The names of the maintainers of `document`.
    pub fn maintainers_of(&self, document: DocumentId) -> &[String] {
        self.maintainers.get(&document).map_or(&[], Vec::as_slice)
    }

    /// Makes `name` a maintainer of `document`, or no longer (`on`).
    pub fn set_maintainer(
        &mut self,
        document: DocumentId,
        name: &str,
        on: bool,
    ) -> Result<(), String> {
        let user = self
            .by_name(name)
            .ok_or_else(|| format!("No user is called {name}"))?;
        if on && user.role < Role::Maintainer {
            return Err(format!(
                "{} is a {}: only maintainers and admins maintain documents",
                user.name, user.role
            ));
        }
        let name = user.name.clone();
        let list = self.maintainers.entry(document).or_default();
        list.retain(|n| !n.eq_ignore_ascii_case(&name));
        if on {
            list.push(name);
        }
        if list.is_empty() {
            self.maintainers.remove(&document);
        }
        self.check()
    }

    /// Forgets the user called `name`, and what they maintain.
    pub fn remove(&mut self, name: &str) -> Result<User, String> {
        let i = self
            .users
            .iter()
            .position(|u| u.name.eq_ignore_ascii_case(name.trim()))
            .ok_or_else(|| format!("No user is called {name}"))?;
        let user = self.users.remove(i);
        for list in self.maintainers.values_mut() {
            list.retain(|n| !n.eq_ignore_ascii_case(&user.name));
        }
        self.maintainers.retain(|_, l| !l.is_empty());
        Ok(user)
    }

    /// Renames the user called `name`, here and in what they maintain.
    pub fn rename(&mut self, name: &str, to: &str) -> Result<(), String> {
        let to = to.trim();
        let Some(user) = self
            .users
            .iter_mut()
            .find(|u| u.name.eq_ignore_ascii_case(name.trim()))
        else {
            return Err(format!("No user is called {name}"));
        };
        let old = std::mem::replace(&mut user.name, to.to_string());
        for list in self.maintainers.values_mut() {
            for n in list.iter_mut().filter(|n| n.eq_ignore_ascii_case(&old)) {
                *n = to.to_string();
            }
        }
        self.check()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cadrs ID, the same each time for the same `n`.
    fn id(n: u8) -> String {
        cadrs_net::bevy_iroh::iroh::SecretKey::from_bytes(&[n; 32])
            .public()
            .to_string()
    }

    fn user(name: &str, role: Role, ids: &[String]) -> User {
        User {
            name: name.into(),
            role,
            sign_in: None,
            ids: ids.to_vec(),
        }
    }

    #[test]
    fn roles_and_maintainers_say_who_may_do_what() {
        let doc = DocumentId::new();
        let other = DocumentId::new();
        let mut users = Users {
            users: vec![
                user("Ada", Role::Admin, &[]),
                user("Max", Role::Maintainer, &[id(1)]),
                user("Cleo", Role::Collaborator, &[id(2)]),
            ],
            ..Users::default()
        };
        assert_eq!(users.role(&id(2)), Some(Role::Collaborator));
        assert_eq!(users.role("someone else"), None);
        assert!(!users.maintains(&id(1), doc));
        users.set_maintainer(doc, "max", true).unwrap();
        assert!(users.maintains(&id(1), doc));
        assert!(!users.maintains(&id(1), other));
        assert_eq!(users.maintainers_of(doc), ["Max"]);
        // A collaborator maintains nothing.
        assert!(users.set_maintainer(doc, "Cleo", true).is_err());
        // Renamed and removed, everywhere.
        users.rename("Max", "Maxine").unwrap();
        assert_eq!(users.maintainers_of(doc), ["Maxine"]);
        users.remove("maxine").unwrap();
        assert!(users.maintainers.is_empty());
    }

    #[test]
    fn a_users_file_holds_one_person_per_name_and_id() {
        let mut users = Users {
            users: vec![user("Ada", Role::Admin, &[id(1)])],
            ..Users::default()
        };
        assert!(users.check().is_ok());
        users.users.push(user("ADA", Role::Collaborator, &[]));
        assert!(users.check().is_err());
        users.users[1] = user("Bob", Role::Collaborator, &[id(1)]);
        assert!(users.check().is_err());
        users.users[1] = user("Bob", Role::Collaborator, &["not an ID".to_string()]);
        assert!(users.check().is_err());
        users.users[1] = user("Bob", Role::Collaborator, &[id(2)]);
        assert!(users.check().is_ok());
    }
}
