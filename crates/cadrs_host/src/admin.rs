//! The commands that look after a host, run on its machine (over SSH, or `docker exec`), with
//! the host running or not: it reads its users again within seconds.
//!
//! - `cadrs --host-users`: the users, with their roles, sign-ins and cadrs IDs.
//! - `cadrs --host-user <name> [--role admin|maintainer|collaborator] [--sign-in <address>]
//!   [--id <cadrs ID>]... [--forget-id <cadrs ID>]... [--rename <new name>] [--remove]`: adds
//!   a user, or changes or removes one.
//! - `cadrs --host-documents`: the documents, with their maintainers and who can open them.
//! - `cadrs --host-document <name or id> [--maintainer <user>]... [--not-maintainer <user>]...`:
//!   one document, and who maintains it.
//!
//! Each takes `--data-dir <dir>`, as `cadrs --host` does.

use std::path::{Path, PathBuf};

use cadrs_core::library::DocumentEntry;
use cadrs_core::{DocumentId, Store};

use crate::users::{Role, User, Users};

pub struct Admin {
    dir: PathBuf,
    command: Command,
}

enum Command {
    Users,
    User {
        name: String,
        role: Option<Role>,
        sign_in: Option<String>,
        ids: Vec<String>,
        forget: Vec<String>,
        rename: Option<String>,
        remove: bool,
    },
    Documents,
    Document {
        which: String,
        maintainers: Vec<(String, bool)>,
    },
}

/// `Some` when the arguments ask for one of the commands.
pub fn parse(args: &[String]) -> Option<Result<Admin, String>> {
    args.iter().find(|a| a.starts_with("--host-"))?;
    Some(parse_command(args))
}

fn parse_command(args: &[String]) -> Result<Admin, String> {
    let mut dir = std::env::var_os("CADRS_DATA_DIR").map(PathBuf::from);
    let mut command = None;
    let (mut role, mut sign_in, mut rename, mut remove) = (None, None, None, false);
    let (mut ids, mut forget, mut maintainers) = (Vec::new(), Vec::new(), Vec::new());
    // The options given, to check they go with the command.
    let mut options = Vec::new();
    let mut it = args.iter();
    let value = |flag: &str, it: &mut std::slice::Iter<String>| {
        it.next()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty() || flag == "--sign-in")
            .ok_or_else(|| format!("{flag} needs a value"))
    };
    while let Some(a) = it.next() {
        let a = a.as_str();
        let new = match a {
            "--host-users" => Some(Command::Users),
            "--host-documents" => Some(Command::Documents),
            "--host-user" => Some(Command::User {
                name: value(a, &mut it)?,
                role: None,
                sign_in: None,
                ids: Vec::new(),
                forget: Vec::new(),
                rename: None,
                remove: false,
            }),
            "--host-document" => Some(Command::Document {
                which: value(a, &mut it)?,
                maintainers: Vec::new(),
            }),
            _ => None,
        };
        if let Some(new) = new {
            if command.is_some() {
                return Err("one --host-... command at a time".into());
            }
            command = Some(new);
            continue;
        }
        match a {
            "--data-dir" => dir = Some(PathBuf::from(value(a, &mut it)?)),
            "--role" => {
                let r = value(a, &mut it)?;
                role = Some(Role::parse(&r).ok_or_else(|| {
                    format!("{r}: the roles are admin, maintainer and collaborator")
                })?);
            }
            "--sign-in" => sign_in = Some(value(a, &mut it)?),
            "--id" => ids.push(value(a, &mut it)?),
            "--forget-id" => forget.push(value(a, &mut it)?),
            "--rename" => rename = Some(value(a, &mut it)?),
            "--remove" => remove = true,
            "--maintainer" => maintainers.push((value(a, &mut it)?, true)),
            "--not-maintainer" => maintainers.push((value(a, &mut it)?, false)),
            other => return Err(format!("{other}: not an option of the --host-... commands")),
        }
        if a != "--data-dir" {
            options.push(a);
        }
    }
    let mut command = command.ok_or("no --host-... command")?;
    let allowed: &[&str] = match &mut command {
        Command::User {
            role: r,
            sign_in: s,
            ids: i,
            forget: f,
            rename: n,
            remove: d,
            ..
        } => {
            (*r, *s, *i, *f, *n, *d) = (role, sign_in, ids, forget, rename, remove);
            &[
                "--role",
                "--sign-in",
                "--id",
                "--forget-id",
                "--rename",
                "--remove",
            ]
        }
        Command::Document { maintainers: m, .. } => {
            *m = maintainers;
            &["--maintainer", "--not-maintainer"]
        }
        _ => &[],
    };
    if let Some(o) = options.iter().find(|o| !allowed.contains(o)) {
        return Err(format!("{o} doesn't go with that command"));
    }
    let dir = dir
        .or_else(crate::default_dir)
        .ok_or("No data dir: give one with --data-dir")?;
    Ok(Admin { dir, command })
}

/// Runs the command: the process's exit code.
pub fn run(admin: Admin) -> u8 {
    let dir = &admin.dir;
    let done = match admin.command {
        Command::Users => Users::read(dir).map(|users| {
            println!(
                "{} users of the host in {}",
                users.users.len(),
                dir.display()
            );
            let names = document_names(dir);
            for u in &users.users {
                print_user(&users, u, &names);
            }
        }),
        Command::User {
            name,
            role,
            sign_in,
            ids,
            forget,
            rename,
            remove,
        } => Users::edit(dir, |users| {
            if remove {
                return users
                    .remove(&name)
                    .map(|u| (format!("Removed {}", u.name), None));
            }
            let added = users.by_name(&name).is_none();
            if added {
                users.users.push(User {
                    name: name.clone(),
                    role: role.unwrap_or(Role::Collaborator),
                    sign_in: None,
                    ids: Vec::new(),
                });
            }
            let user = users
                .users
                .iter_mut()
                .find(|u| u.name.eq_ignore_ascii_case(&name))
                .expect("just added");
            if let Some(role) = role {
                user.role = role;
            }
            if let Some(s) = sign_in {
                user.sign_in = (!s.is_empty()).then_some(s);
            }
            for id in ids {
                if !user.ids.contains(&id) {
                    user.ids.push(id);
                }
            }
            for id in forget {
                if !user.ids.contains(&id) {
                    return Err(format!("{} doesn't use {id}", user.name));
                }
                user.ids.retain(|i| *i != id);
            }
            let (current, role) = (user.name.clone(), user.role);
            // A collaborator maintains nothing.
            if role < Role::Maintainer {
                for list in users.maintainers.values_mut() {
                    list.retain(|n| !n.eq_ignore_ascii_case(&current));
                }
                users.maintainers.retain(|_, l| !l.is_empty());
            }
            let current = match rename {
                Some(to) => {
                    users.rename(&current, &to)?;
                    to
                }
                None => current,
            };
            let said = if added { "Added" } else { "Changed" };
            Ok((format!("{said}:"), Some(current)))
        })
        .map(|(users, (said, name))| {
            println!("{said}");
            if let Some(u) = name.and_then(|n| users.by_name(&n)) {
                print_user(&users, u, &document_names(dir));
            }
        }),
        Command::Documents => Users::read(dir).map(|users| {
            let mine = owned(dir);
            println!("{} documents on the host in {}", mine.len(), dir.display());
            for e in &mine {
                print_document(&users, e);
            }
        }),
        Command::Document { which, maintainers } => find(dir, &which).and_then(|e| {
            let users = if maintainers.is_empty() {
                Users::read(dir)?
            } else {
                Users::edit(dir, |users| {
                    for (name, on) in &maintainers {
                        users.set_maintainer(e.id, name, *on)?;
                    }
                    Ok(())
                })?
                .0
            };
            print_document(&users, &e);
            Ok(())
        }),
    };
    match done {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

/// The documents the host owns (not copies of others' documents), newest first.
fn owned(dir: &Path) -> Vec<DocumentEntry> {
    let (library, errors) = Store::new(dir).list();
    for (path, e) in errors {
        eprintln!("cannot read {}: {e}", path.display());
    }
    let mut mine: Vec<DocumentEntry> = library
        .entries
        .into_iter()
        .filter(|e| e.meta.shared.is_none())
        .collect();
    mine.sort_by_key(|e| std::cmp::Reverse(e.meta.modified));
    mine
}

fn document_names(dir: &Path) -> Vec<(DocumentId, String)> {
    owned(dir).into_iter().map(|e| (e.id, e.name)).collect()
}

/// The document named `which` (its name, whatever the case, or its id).
fn find(dir: &Path, which: &str) -> Result<DocumentEntry, String> {
    let mut found: Vec<DocumentEntry> = owned(dir)
        .into_iter()
        .filter(|e| e.id.to_string() == which || e.name.eq_ignore_ascii_case(which))
        .collect();
    match found.len() {
        0 => Err(format!("The host has no document {which}")),
        1 => Ok(found.remove(0)),
        _ => Err(format!(
            "Several documents are called {which}: give one of their ids, {}",
            found
                .iter()
                .map(|e| e.id.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn print_user(users: &Users, u: &User, names: &[(DocumentId, String)]) {
    let sign_in = u
        .sign_in
        .as_deref()
        .map(|s| format!(", signs in as {s}"))
        .unwrap_or_default();
    println!("{} ({}{sign_in})", u.name, u.role);
    let maintains: Vec<&str> = names
        .iter()
        .filter(|(id, _)| {
            users
                .maintainers_of(*id)
                .iter()
                .any(|n| n.eq_ignore_ascii_case(&u.name))
        })
        .map(|(_, n)| n.as_str())
        .collect();
    if !maintains.is_empty() {
        println!("    maintains {}", maintains.join(", "));
    }
    if u.ids.is_empty() {
        println!("    no cadrs ID yet: add one with --id");
    }
    for id in &u.ids {
        println!("    {id}");
    }
}

fn print_document(users: &Users, e: &DocumentEntry) {
    let trashed = if e.meta.trashed.is_some() {
        "  (in the trash)"
    } else {
        ""
    };
    println!("{}  {}{trashed}", e.id, e.name);
    let maintainers = users.maintainers_of(e.id);
    if maintainers.is_empty() {
        println!("    no maintainers (the admins manage it)");
    } else {
        println!("    maintained by {}", maintainers.join(", "));
    }
    let Some(sharing) = &e.meta.sharing else {
        println!("    not shared: the host keeps it, nobody can open it");
        return;
    };
    for m in &sharing.members {
        let who = match users.by_id(&m.id) {
            Some(u) if u.name != m.name => format!("{} ({})", m.name, u.name),
            Some(_) => m.name.clone(),
            None => format!("{} (not a user, so can't open it: {})", m.name, m.id),
        };
        println!("    {who}: {}", m.permission.label());
    }
    if let Some(p) = sharing.link {
        println!("    anyone with the link who is a user: {}", p.label());
    }
}
