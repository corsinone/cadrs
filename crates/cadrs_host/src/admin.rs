//! The commands that look after a host, run on its machine (over SSH, or `docker exec`), with
//! the host running or not: it reads its users again within seconds.
//!
//! - `cadrs --host-users`: the users, with their roles, sign-ins and cadrs IDs.
//! - `cadrs --host-user <name> [--role admin|maintainer|collaborator] [--sign-in <address>]
//!   [--id <cadrs ID>]... [--forget-id <cadrs ID>]... [--rename <new name>] [--remove]`: adds
//!   a user, or changes or removes one.
//! - `cadrs --host-documents`: the documents, with their maintainers and who can open them.
//! - `cadrs --host-document <name or id> [--maintainer <user>]... [--not-maintainer <user>]...
//!   [--edit <user>]... [--view <user>]... [--unshare <user or cadrs ID>]...
//!   [--link off|view|edit]`: one document, who maintains it and whom it is shared with.
//! - `cadrs --host-settings [--public on|off]`: the host's settings.
//! - `cadrs --host-backup <folder>`: a consistent copy of the data dir ([`backup`]).
//!
//! Each takes `--data-dir <dir>`, as `cadrs --host` does.
//!
//! A change to whom a document is shared with goes to the host as a file in `changes/` in its
//! data dir ([`SharingChange`]), which it makes as it scans, as it would the same change asked
//! by a peer: everyone in the room hears the new access, new members are sent the link, and
//! the document moves to a fresh room when someone is removed from it without link sharing,
//! or link sharing is turned off (as the app does). A host that isn't running makes it when it
//! starts.

use std::path::{Path, PathBuf};
use std::time::Duration;

use cadrs_core::collab::{Member, Permission, Sharing};
use cadrs_core::history_log::HistoryLog;
use cadrs_core::library::DocumentEntry;
use cadrs_core::{DocumentId, Store};
use cadrs_net::bevy_iroh::iroh::EndpointId;

use crate::users::{Role, User, Users};

/// Where the sharing changes asked from the command line wait for the host, in its data dir.
pub const CHANGES_DIR: &str = "changes";

/// How long `--host-document` waits for a running host to make its change.
const WAIT_FOR_HOST: Duration = Duration::from_secs(15);

/// Changes to whom a document is shared with, for the host to make.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SharingChange {
    pub document: DocumentId,
    pub what: Vec<Share>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Share {
    /// Share it with a user (each of their cadrs IDs), or change what they may do.
    With {
        user: String,
        permission: Permission,
    },
    /// No longer share it with a user, or with one cadrs ID.
    Not(String),
    /// Link sharing: anyone with the link who is a user gets this; `None` turns it off.
    Link(Option<Permission>),
}

/// Makes `what` to `sharing`, all or nothing: whether the document has to move to a fresh room
/// now, and what was done, for the log.
pub(crate) fn share(
    sharing: &mut Sharing,
    users: &Users,
    what: &[Share],
) -> Result<(bool, Vec<String>), String> {
    let mut s = sharing.clone();
    let (mut rotate, mut said) = (false, Vec::new());
    for w in what {
        match w {
            Share::With { user, permission } => {
                let u = users
                    .by_name(user)
                    .ok_or_else(|| format!("No user is called {user}"))?;
                if u.ids.is_empty() {
                    return Err(format!("{} has no cadrs ID yet", u.name));
                }
                for id in &u.ids {
                    match s.members.iter_mut().find(|m| m.id == *id) {
                        Some(m) => {
                            m.permission = *permission;
                            m.rights = None;
                        }
                        None => s.members.push(Member {
                            id: id.clone(),
                            name: u.name.clone(),
                            permission: *permission,
                            rights: None,
                            delivered: None,
                        }),
                    }
                }
                said.push(format!("{}: {}", u.name, permission.label()));
            }
            Share::Not(who) => {
                let ids = match users.by_name(who) {
                    Some(u) => u.ids.clone(),
                    None if who.parse::<EndpointId>().is_ok() => vec![who.clone()],
                    None => return Err(format!("No user is called {who}")),
                };
                let before = s.members.len();
                s.members.retain(|m| !ids.contains(&m.id));
                if s.members.len() == before {
                    return Err(format!("It isn't shared with {who}"));
                }
                rotate |= s.link.is_none();
                said.push(format!("no longer shared with {who}"));
            }
            Share::Link(p) => {
                rotate |= s.link.is_some() && p.is_none();
                s.link = *p;
                said.push(match p {
                    Some(p) => format!("anyone with the link who is a user: {}", p.label()),
                    None => "link sharing off".into(),
                });
            }
        }
    }
    *sharing = s;
    Ok((rotate, said))
}

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
        sharing: Vec<Share>,
    },
    Settings {
        public: Option<bool>,
    },
    Backup {
        to: PathBuf,
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
    let (mut sharing, mut public) = (Vec::new(), None);
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
                sharing: Vec::new(),
            }),
            "--host-settings" => Some(Command::Settings { public: None }),
            "--host-backup" => Some(Command::Backup {
                to: PathBuf::from(value(a, &mut it)?),
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
            "--edit" | "--view" => sharing.push(Share::With {
                user: value(a, &mut it)?,
                permission: if a == "--edit" {
                    Permission::Edit
                } else {
                    Permission::View
                },
            }),
            "--unshare" => sharing.push(Share::Not(value(a, &mut it)?)),
            "--link" => sharing.push(Share::Link(match value(a, &mut it)?.as_str() {
                "off" => None,
                "view" => Some(Permission::View),
                "edit" => Some(Permission::Edit),
                other => return Err(format!("--link {other}: off, view or edit")),
            })),
            "--public" => {
                public = Some(match value(a, &mut it)?.as_str() {
                    "on" => true,
                    "off" => false,
                    other => return Err(format!("--public {other}: on or off")),
                })
            }
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
        Command::Document {
            maintainers: m,
            sharing: s,
            ..
        } => {
            (*m, *s) = (maintainers, sharing);
            &[
                "--maintainer",
                "--not-maintainer",
                "--edit",
                "--view",
                "--unshare",
                "--link",
            ]
        }
        Command::Settings { public: p } => {
            *p = public;
            &["--public"]
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
        Command::Document {
            which,
            maintainers,
            sharing,
        } => find(dir, &which).and_then(|mut e| {
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
            if !sharing.is_empty() {
                // Checked here, so a mistake is said now; made by the host.
                let mut check = e.meta.sharing.clone().unwrap_or(Sharing::new([0; 32]));
                share(&mut check, &users, &sharing)?;
                let change = SharingChange {
                    document: e.id,
                    what: sharing,
                };
                if ask_host(dir, &change)? {
                    e = find(dir, &e.id.to_string())?;
                } else {
                    println!(
                        "The host isn't running (or is busy): it makes the change when it starts."
                    );
                    return Ok(());
                }
            }
            print_document(&users, &e);
            Ok(())
        }),
        Command::Settings { public } => {
            let set = public.map_or(Ok(()), |on| crate::set_public(dir, on));
            set.and_then(|_| crate::read_settings(dir)).map(|s| {
                let on = if s.public { "on" } else { "off" };
                println!("Settings of the host in {}", dir.display());
                println!("    public room: {on} (--public on|off; a running host follows it)");
            })
        }
        Command::Backup { to } => backup(dir, &to).map(|n| {
            println!(
                "Copied {n} documents, and the host's identity, settings and users, to {}.\n\
                 The copy holds the host's key (identity.key): keep it as private as the host.",
                to.display()
            );
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

/// Gives `change` to the host, and waits for it to be made: whether it was.
fn ask_host(dir: &Path, change: &SharingChange) -> Result<bool, String> {
    let changes = dir.join(CHANGES_DIR);
    cadrs_fs::create_dir_all(&changes).map_err(|e| format!("{}: {e}", changes.display()))?;
    // Named so the host makes them in the order they were asked.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let path = changes.join(format!("{nanos:024}-{}.ron", uuid::Uuid::new_v4()));
    let text = ron::ser::to_string_pretty(change, ron::ser::PrettyConfig::default())
        .map_err(|e| e.to_string())?;
    let tmp = path.with_extension("tmp");
    cadrs_fs::write(&tmp, text)
        .and_then(|_| cadrs_fs::rename(&tmp, &path))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let start = std::time::Instant::now();
    while start.elapsed() < WAIT_FOR_HOST {
        if !cadrs_fs::exists(&path) {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(false)
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

/// How many times a document's folder is copied again when the host keeps changing it.
const TRIES: usize = 10;

/// Copies the data dir of a host, `dir`, into `to` (a new folder, or an empty one), running or
/// not: each file as it was last written whole (the host writes a file next to it, then puts
/// it in its place, so the half-written ones are left out), and each document's folder copied
/// again if the host changed it meanwhile, so its files go together. Then every document in the
/// copy is read back. Returns how many there are. An external tool keeps the copies (one
/// folder each); one restores by stopping the host and putting a copy in place of its data
/// dir, and can test one first with `cadrs --document <name> --check --data-dir <copy>`.
pub fn backup(dir: &Path, to: &Path) -> Result<usize, String> {
    let fail = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    if !cadrs_fs::is_dir(dir) {
        return Err(format!("{} is not a host's data dir", dir.display()));
    }
    if cadrs_fs::exists(to)
        && cadrs_fs::read_dir(to)
            .map_err(|e| fail(to, e))?
            .next()
            .is_some()
    {
        return Err(format!("{} is not empty", to.display()));
    }
    cadrs_fs::create_dir_all(to).map_err(|e| fail(to, e))?;
    let (from_c, to_c) = (
        cadrs_fs::canonicalize(dir).map_err(|e| fail(dir, e))?,
        cadrs_fs::canonicalize(to).map_err(|e| fail(to, e))?,
    );
    if to_c.starts_with(&from_c) {
        let _ = cadrs_fs::remove_dir(to);
        return Err(format!("{} is inside the data dir", to.display()));
    }
    let mut entries: Vec<PathBuf> = cadrs_fs::read_dir(dir)
        .map_err(|e| fail(dir, e))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| !is_temporary(p))
        .collect();
    entries.sort();
    for from in entries {
        let into = to.join(from.file_name().expect("an entry of a folder"));
        copy_unchanged(&from, &into)?;
    }
    // What a restore will read.
    let store = Store::new(to);
    let (library, errors) = store.list();
    if let Some((path, e)) = errors.first() {
        return Err(format!("the copy of {} can't be read: {e}", path.display()));
    }
    for e in &library.entries {
        store
            .load(e.id)
            .map_err(|err| format!("the copy of {} can't be read: {err}", e.name))?;
        HistoryLog::load(&store, e.id)
            .map_err(|err| format!("the copy of {}'s history can't be read: {err}", e.name))?;
    }
    Users::read(to)?.check()?;
    crate::read_settings(to)?;
    Ok(library.entries.len())
}

/// A file the host is writing, about to replace the one it is named after.
fn is_temporary(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "tmp")
}

/// Copies `from` (a file or a folder) to `to`, again until nothing in it changed meanwhile.
fn copy_unchanged(from: &Path, to: &Path) -> Result<(), String> {
    for _ in 0..TRIES {
        let before = stamps(from)?;
        copy(from, to)?;
        if stamps(from)? == before {
            return Ok(());
        }
        let _ = if cadrs_fs::is_dir(to) {
            cadrs_fs::remove_dir_all(to)
        } else {
            cadrs_fs::remove_file(to)
        };
        std::thread::sleep(Duration::from_millis(200));
    }
    Err(format!(
        "{} kept changing while it was copied",
        from.display()
    ))
}

/// Each file under `path` (or `path` itself) with its size and when it was written.
fn stamps(path: &Path) -> Result<Vec<(PathBuf, u64, Option<cadrs_fs::SystemTime>)>, String> {
    let mut out = Vec::new();
    let mut todo = vec![path.to_path_buf()];
    while let Some(p) = todo.pop() {
        if is_temporary(&p) {
            continue;
        }
        let Ok(m) = cadrs_fs::metadata(&p) else {
            // Gone meanwhile: the copy will say so.
            continue;
        };
        if m.is_dir() {
            let read = cadrs_fs::read_dir(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            todo.extend(read.filter_map(|e| e.ok().map(|e| e.path())));
        } else {
            out.push((p, m.len(), m.modified().ok()));
        }
    }
    out.sort();
    Ok(out)
}

fn copy(from: &Path, to: &Path) -> Result<(), String> {
    let fail = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    if !cadrs_fs::is_dir(from) {
        return match cadrs_fs::copy(from, to) {
            // Gone meanwhile (replaced by the host): the folder is copied again.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            done => done.map(|_| ()).map_err(|e| fail(from, e)),
        };
    }
    cadrs_fs::create_dir_all(to).map_err(|e| fail(to, e))?;
    for e in cadrs_fs::read_dir(from).map_err(|e| fail(from, e))? {
        let p = e.map_err(|e| fail(from, e))?.path();
        if !is_temporary(&p) {
            copy(&p, &to.join(p.file_name().expect("an entry of a folder")))?;
        }
    }
    Ok(())
}
