//! `cadrs --host [--data-dir <dir>] [--name <name>]`: the always-online owner of a team's
//! documents, with no window.
//!
//! A host is a peer like any other ([`cadrs_net`]), which owns the shared documents in its
//! data dir and does for all of them at once what the app does for the one document open in
//! it (`cadrs_app::collab::live`): it keeps each document's room, sends the document to whoever
//! may join, and numbers the edits the others propose ([`Sequencer`]), first come first
//! served, refusing one that changes what someone else changed first. It has no kernel:
//! numbering edits compares what they write, it rebuilds nothing.
//!
//! Its identity and its documents live in its data dir: `--data-dir`, else `CADRS_DATA_DIR`,
//! else `host` next to the app's documents (`~/.local/share/cadrs/host`), never the app's own.
//! It is the same peer from run to run, and prints its cadrs ID, which is what a document is
//! shared with and transferred to.
//!
//! Each numbered edit goes into the document's history log, as its author's, the way the
//! owner's app records it; versions are made there (**Create version** by whoever may edit),
//! and comments are kept next to the document. It keeps the tab locks too ("Open tab, close
//! it for other users"): it grants them, refuses the others' edits to a held tab, and lets a
//! lock go when its holder has left the tab. Whoever may share asks it to add people, and
//! members leave; everyone in the room hears the new access, and each member is sent the link
//! until their cadrs takes it (as the owner's app does while the document is open). It
//! answers Join, Propose, Status, Comment, CreateVersion, Lock, Unlock, AddMember and Leave,
//! and takes documents back by Handover from whoever kept the edits in order while the host
//! was away.
//!
//! It answers only its users ([`users`]: `users.ron` in its data dir, edited with
//! `cadrs --host-user`), people with a role and the cadrs IDs they use; anyone else is refused
//! whatever they ask, link or no link, and isn't sent links either. Documents come to the host
//! by **Transfer ownership** from a maintainer or an admin: they share the document with the
//! host and transfer it, the host sends everyone it is shared with the new link, and they
//! maintain it from then on. The maintainers of a document and the admins may share it with
//! more people (users only) whatever their own access. [`admin`] has the commands that list
//! and change all of this (whom a document is shared with too, which the host then makes as
//! it would for a peer), and the backup.
//!
//! The host stays out of the public room (public documents, the check-in at peers.cadrs.net)
//! unless an admin turns it on (`--public on`, kept in `host.ron`): it is there for the team's
//! private documents, and the seed peer does the public ones.

pub mod admin;
pub mod users;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bevy::app::ScheduleRunnerPlugin;
use bevy::prelude::*;
use cadrs_core::collab::comments::{CommentOp, Comments};
use cadrs_core::collab::sync::{Change, Key, Refusal, Sequencer};
use cadrs_core::collab::{Access, Member, Permission, Rights, Snapshot};
use cadrs_core::history_log::{HistoryLog, Origin, VersionId, document_hash};
use cadrs_core::library::DocumentMeta;
use cadrs_core::{Document, DocumentId, ElementId, Store};
use cadrs_net::bevy_iroh::iroh::EndpointId;
use cadrs_net::bevy_iroh::prelude::{NetSender, Peer, PeerJoined, Received};
use cadrs_net::bevy_iroh::{Iroh, Relays};
use cadrs_net::{
    AccessChanged, Answer, Ask, CommentChanged, Invite, LocksChanged, Presence, Request, Requests,
    Sequenced, ShareLink, Verdict, VersionCreated, Welcome, session, transfer,
};
use users::{Role, User, Users};

/// How often the data dir is read again for documents that came, went or changed rooms.
const SCAN_EVERY: f64 = 5.0;
/// How long a document waits after an edit before it is saved (a drag sends several a second).
const SAVE_AFTER: f64 = 1.0;
/// How often the host says how it is doing.
const REPORT_EVERY: f64 = 300.0;
/// How long a lock's holder may be away from its tab before the host lets it go (the app's
/// owner waits as long).
const AWAY: f64 = 3.0;
/// Someone not heard from this long has left (their presence comes every few seconds).
const FORGET_AFTER: f64 = 15.0;
/// How often the link goes out again to members whose cadrs hasn't taken it (the app's
/// owner tries as often).
const RETRY_INVITES: f64 = 30.0;

pub struct Host {
    dir: PathBuf,
    name: String,
    /// `--public on|off`: to change whether the host is in the public room.
    public: Option<bool>,
}

/// The host's settings, in its data dir.
pub const SETTINGS_FILE: &str = "host.ron";

/// What `host.ron` holds.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Settings {
    /// The admins' cadrs IDs, of a host from before it had users: they become admins in
    /// `users.ron` when it starts ([`adopt_admins`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    admins: Vec<String>,
    /// Whether the host takes part in public documents (the public room); off unless an admin
    /// turns it on.
    #[serde(default)]
    public: bool,
}

fn read_settings(dir: &Path) -> Result<Settings, String> {
    let path = dir.join(SETTINGS_FILE);
    if !cadrs_fs::exists(&path) {
        return Ok(Settings::default());
    }
    let text = cadrs_fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    ron::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Makes the admins in `host.ron` of a host from before it had users admin users, named
/// "Admin 1", "Admin 2"...; their names.
fn adopt_admins(dir: &Path) -> Result<Vec<String>, String> {
    let ids = read_settings(dir)?.admins;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let (_, names) = Users::edit(dir, |users| {
        let mut names = Vec::new();
        for id in ids {
            if users.by_id(&id).is_some() || id.parse::<EndpointId>().is_err() {
                continue;
            }
            let name = (1..)
                .map(|n| format!("Admin {n}"))
                .find(|n| users.by_name(n).is_none())
                .unwrap_or_default();
            users.users.push(User {
                name: name.clone(),
                role: Role::Admin,
                sign_in: None,
                ids: vec![id],
            });
            names.push(name);
        }
        Ok(names)
    })?;
    edit_settings(dir, |s| s.admins.clear())?;
    Ok(names)
}

/// Turns the public room on or off for the host whose data dir is `dir`.
pub fn set_public(dir: &Path, on: bool) -> Result<(), String> {
    edit_settings(dir, |s| s.public = on)
}

fn edit_settings(dir: &Path, f: impl FnOnce(&mut Settings)) -> Result<(), String> {
    let mut settings = read_settings(dir)?;
    f(&mut settings);
    let text = ron::ser::to_string_pretty(&settings, ron::ser::PrettyConfig::default())
        .map_err(|e| e.to_string())?;
    let path = dir.join(SETTINGS_FILE);
    cadrs_fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))
}

/// `Some` when the arguments ask for a host.
pub fn parse(args: &[String]) -> Option<Result<Host, String>> {
    if !args.iter().any(|a| a == "--host") {
        return None;
    }
    let mut dir = std::env::var_os("CADRS_DATA_DIR").map(PathBuf::from);
    let mut name = "cadrs host".to_string();
    let mut public = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--host" => {}
            "--data-dir" => match it.next() {
                Some(d) => dir = Some(PathBuf::from(d)),
                None => return Some(Err("--data-dir needs a folder".into())),
            },
            "--name" => match it.next() {
                Some(n) if !n.trim().is_empty() => name = n.trim().to_string(),
                _ => return Some(Err("--name needs a name".into())),
            },
            "--public" => match it.next().map(String::as_str) {
                Some("on") => public = Some(true),
                Some("off") => public = Some(false),
                _ => return Some(Err("--public needs on or off".into())),
            },
            other => {
                return Some(Err(format!(
                    "{other}: a host takes only --data-dir, --name and --public (its users: \
                     cadrs --host-user)"
                )));
            }
        }
    }
    Some(match dir.or_else(default_dir) {
        Some(dir) => Ok(Host { dir, name, public }),
        None => Err("No data dir: give one with --data-dir".into()),
    })
}

/// Where a host keeps everything when no `--data-dir` says: `host` next to the app's documents.
pub fn default_dir() -> Option<PathBuf> {
    Store::default_root().map(|r| r.parent().map_or(r.clone(), |p| p.join("host")))
}

/// One document the host owns and shares: its room, and the numbering of its edits.
struct Hosted {
    name: String,
    room: Entity,
    topic: [u8; 32],
    /// This session: edits are numbered from 0 each time the host opens the document.
    epoch: u64,
    seq: Sequencer,
    /// The document after the last numbered edit.
    confirmed: Box<Document>,
    /// Its history, up to `confirmed`.
    log: HistoryLog,
    meta: DocumentMeta,
    /// When the first edit not saved yet was numbered.
    unsaved: Option<f64>,
    /// The tabs someone has open for themselves: each with its holder's cadrs ID and name.
    locks: Vec<(ElementId, String, String)>,
    /// Since when each lock's holder has been away from its tab.
    away: Vec<(ElementId, f64)>,
    /// The tab each peer in the room is in, by cadrs ID, and when they were last heard from.
    present: HashMap<String, (Option<ElementId>, f64)>,
    /// The links on their way to members, by cadrs ID: the room each names, and the answer.
    invites: HashMap<String, ([u8; 32], Answer<Verdict>)>,
}

impl Hosted {
    /// Numbers `change` (made on edit `base`) by `author`: it goes into the history log, is
    /// saved soon, and is what the room is told.
    fn number(
        &mut self,
        document: DocumentId,
        (author, author_name): (&str, &str),
        id: u64,
        base: u64,
        change: Change,
        t: f64,
    ) -> Result<(u64, Outgoing), Refusal> {
        let (seq, change) = self.seq.admit(author, base, change, &mut self.confirmed)?;
        let m = Sequenced::new(
            document,
            self.epoch,
            seq,
            (author, author_name),
            id,
            &change,
            document_hash(&self.confirmed),
        );
        self.log.record(
            &self.confirmed,
            Origin::Command(String::new()),
            unix_now(),
            author_name,
        );
        self.unsaved.get_or_insert(t);
        Ok((seq, Outgoing::Edit(m)))
    }

    /// The holder of the lock on `key`'s tab, when it is someone other than `by`: their name.
    fn held_against(&self, key: Key, by: &str) -> Option<&str> {
        let element = match key {
            Key::Head => return None,
            Key::Element(e) | Key::Shell(e) | Key::Feature(e, _) => e,
        };
        self.locks
            .iter()
            .find(|(e, id, _)| *e == element && id != by)
            .map(|(.., name)| name.as_str())
    }

    fn access_changed(&self, document: DocumentId) -> Option<Outgoing> {
        let s = self.meta.sharing.as_ref()?;
        Some(Outgoing::Access(AccessChanged {
            document,
            members: s
                .members
                .iter()
                .map(|m| (m.id.clone(), m.access()))
                .collect(),
            link: s.link_access(),
        }))
    }

    fn locks_changed(&self, document: DocumentId) -> Outgoing {
        Outgoing::Locks(LocksChanged {
            document,
            locks: self.locks.clone(),
        })
    }
}

/// The documents the host owns, by id.
#[derive(Resource)]
struct Documents {
    store: Store,
    /// What collaborators see as the owner's name.
    name: String,
    open: HashMap<DocumentId, Hosted>,
    /// When to send the links again (seconds on the real clock): 0 sends them at once.
    next_invites: f64,
    /// Whom it answers, and what they may do (`users.ron`).
    users: Users,
    /// Why `users.ron` could not be read, when it couldn't (said once).
    users_error: Option<String>,
    /// Look at the data dir now, not at the next scan (a document just came).
    rescan: bool,
}

/// What the host tells a room.
enum Outgoing {
    Edit(Sequenced),
    Version(VersionCreated),
    Comment(CommentChanged),
    Locks(LocksChanged),
    Access(AccessChanged),
}

/// Messages on their way to their rooms.
#[derive(Resource, Default)]
struct Outbox(Vec<(Entity, Outgoing)>);

pub fn run(host: Host) -> AppExit {
    if let Err(e) = cadrs_fs::create_dir_all(&host.dir) {
        eprintln!("cannot use {}: {e}", host.dir.display());
        return AppExit::from_code(2);
    }
    match adopt_admins(&host.dir) {
        Ok(names) if !names.is_empty() => println!(
            "The admins in host.ron are users now, {}: rename them with \
             cadrs --host-user <name> --rename <new name>",
            names.join(", ")
        ),
        Ok(_) => {}
        Err(e) => {
            eprintln!("{e}");
            return AppExit::from_code(2);
        }
    }
    if let Some(on) = host.public
        && let Err(e) = set_public(&host.dir, on)
    {
        eprintln!("{e}");
        return AppExit::from_code(2);
    }
    let settings = match read_settings(&host.dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return AppExit::from_code(2);
        }
    };
    let users = match Users::read(&host.dir).and_then(|u| u.check().map(|_| u)) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("{e}");
            return AppExit::from_code(2);
        }
    };
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(Duration::from_secs_f64(
            1.0 / 30.0,
        ))),
        bevy::log::LogPlugin::default(),
    ));
    // Ctrl+C stops the host after a last save, rather than at once. (A page has no terminal;
    // the browser build compiles this crate, though it never runs a host.)
    #[cfg(not(target_arch = "wasm32"))]
    app.add_plugins(bevy::app::TerminalCtrlCHandlerPlugin);
    let identity = Some(host.dir.join(cadrs_net::IDENTITY_FILE));
    add(&mut app, &host.dir, &host.name, identity, Relays::default());
    match cadrs_net::cadrs_id(app.world().get_resource::<Iroh>()) {
        Some(id) => {
            println!(
                "cadrs host \"{}\", keeping {}",
                host.name,
                host.dir.display()
            );
            println!("cadrs ID: {id}");
            let count = |role| users.users.iter().filter(|u| u.role == role).count();
            if count(Role::Admin) == 0 {
                println!(
                    "No admins yet: add yourself with cadrs --host-user <your name> --role admin \
                     --id <your cadrs ID> --data-dir {}",
                    host.dir.display()
                );
            }
            println!(
                "{} users: {} admins, {} maintainers, {} collaborators",
                users.users.len(),
                count(Role::Admin),
                count(Role::Maintainer),
                count(Role::Collaborator)
            );
            if settings.public {
                println!("In the public room (--public off to leave it).");
            }
        }
        None => {
            eprintln!(
                "cannot go online: {}",
                app.world()
                    .get_resource::<cadrs_net::bevy_iroh::StartError>()
                    .map_or("unknown error", |e| e.0.as_str())
            );
            return AppExit::from_code(1);
        }
    }
    app.run()
}

/// Adds the network and the host to `app`: the owner of the shared documents in `dir`, under
/// the identity in `identity` (a fresh one each run when `None`, for tests).
pub fn add(app: &mut App, dir: &Path, name: &str, identity: Option<PathBuf>, relays: Relays) {
    app.add_plugins(cadrs_net::NetPlugin {
        identity,
        display_name: name.into(),
        relays,
        autostart: true,
    });
    let settings = read_settings(dir).unwrap_or_else(|e| {
        error!("{e}");
        Settings::default()
    });
    // (After the network, which turns public documents on.)
    app.insert_resource(cadrs_net::public::PublicEnabled(settings.public));
    let (users, users_error) = match Users::read(dir) {
        Ok(u) => (u, None),
        Err(e) => {
            error!("{e}");
            (Users::default(), Some(e))
        }
    };
    app.insert_resource(Documents {
        store: Store::new(dir),
        name: name.into(),
        open: HashMap::new(),
        next_invites: 0.0,
        users,
        users_error,
        rescan: false,
    })
    .init_resource::<Outbox>()
    .add_systems(
        Update,
        (
            scan,
            hear,
            serve,
            release_left,
            deliver_invites,
            send,
            save,
            report,
        )
            .chain(),
    )
    .add_systems(Last, save_on_exit);
}

fn now(world: &World) -> f64 {
    world
        .resource::<Time<bevy::time::Real>>()
        .elapsed_secs_f64()
}

/// A fresh session number.
fn epoch() -> u64 {
    uuid::Uuid::new_v4().as_u64_pair().0 | 1
}

/// The time the history log and the document's dates are in.
fn unix_now() -> i64 {
    cadrs_net::public::unix_now() as i64
}

/// The document's history log, started if it has none and caught up with the document if it
/// was changed without it (by the app, before it was handed to the host).
fn open_log(store: &Store, doc: &Document, meta: &DocumentMeta, host: &str) -> HistoryLog {
    let loaded = match HistoryLog::load(store, doc.id) {
        Ok(l) => l,
        Err(e) => {
            warn!(
                "cannot read the history of {}: {e}; starting a new one",
                doc.name
            );
            None
        }
    };
    let mut changed = loaded.as_ref().is_none_or(HistoryLog::upgraded);
    let mut log = loaded.unwrap_or_else(|| HistoryLog::start(doc, meta.created, host));
    if log.head() != doc {
        log.catch_up(doc, unix_now(), host);
        changed = true;
    }
    if changed && let Err(e) = log.save(store) {
        error!("cannot save the history of {}: {e}", doc.name);
    }
    log
}

/// Opens a room for each shared document the host owns, and leaves the room of one that is no
/// longer shared, was trashed, or moved to a new room (link sharing turned off).
fn scan(world: &mut World, mut last: Local<Option<f64>>) {
    let t = now(world);
    let rescan = std::mem::take(&mut world.resource_mut::<Documents>().rescan);
    if !rescan && last.is_some_and(|l| t - l < SCAN_EVERY) {
        return;
    }
    *last = Some(t);
    let store = world.resource::<Documents>().store.clone();
    read_users(&mut world.resource_mut::<Documents>());
    // The public room, as an admin last set it (`cadrs --host-settings`).
    if let Ok(settings) = read_settings(store.root())
        && let Some(mut on) = world.get_resource_mut::<cadrs_net::public::PublicEnabled>()
        && on.0 != settings.public
    {
        on.0 = settings.public;
        info!(
            "{} the public room",
            if settings.public {
                "joining"
            } else {
                "leaving"
            }
        );
    }
    let (library, errors) = store.list();
    for (path, e) in errors {
        warn!("cannot read {}: {e}", path.display());
    }
    // What should be open: owned (not someone else's copy), shared, not in the trash.
    let wanted: HashMap<DocumentId, [u8; 32]> = library
        .entries
        .iter()
        .filter(|e| e.meta.shared.is_none() && e.meta.trashed.is_none())
        .filter_map(|e| Some((e.id, e.meta.sharing.as_ref()?.topic)))
        .collect();
    let gone: Vec<DocumentId> = world
        .resource::<Documents>()
        .open
        .iter()
        .filter(|(id, h)| wanted.get(*id) != Some(&h.topic))
        .map(|(id, _)| *id)
        .collect();
    for id in gone {
        close(world, id, &store);
    }
    for (id, topic) in wanted {
        if world.resource::<Documents>().open.contains_key(&id) {
            continue;
        }
        let file = match store.load(id) {
            Ok(f) => f,
            Err(e) => {
                warn!("cannot open {id}: {e}");
                continue;
            }
        };
        let name = file.document.name.clone();
        let host = world.resource::<Documents>().name.clone();
        let log = open_log(&store, &file.document, &file.meta, &host);
        let room = session::host_room(&mut world.commands(), topic, &name);
        world.flush();
        info!("hosting {name} in its room");
        world.resource_mut::<Documents>().open.insert(
            id,
            Hosted {
                name,
                room,
                topic,
                epoch: epoch(),
                seq: Sequencer::new(0),
                confirmed: Box::new(file.document),
                log,
                meta: file.meta,
                unsaved: None,
                locks: Vec::new(),
                away: Vec::new(),
                present: HashMap::new(),
                invites: HashMap::new(),
            },
        );
    }
    take_changes(world);
}

/// Makes the sharing changes admins asked for from the command line (`cadrs --host-document`),
/// in the order they were asked: each a file in `changes/`, gone once made.
fn take_changes(world: &mut World) {
    let dir = world
        .resource::<Documents>()
        .store
        .root()
        .join(admin::CHANGES_DIR);
    let Ok(read) = cadrs_fs::read_dir(&dir) else {
        return;
    };
    let mut files: Vec<PathBuf> = read
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "ron"))
        .collect();
    files.sort();
    for path in files {
        let change = cadrs_fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|t| ron::from_str::<admin::SharingChange>(&t).map_err(|e| e.to_string()))
            .and_then(|c| change_sharing(world, &c));
        if let Err(e) = cadrs_fs::remove_file(&path) {
            error!("cannot take {}: {e}", path.display());
        }
        match change {
            Ok(said) => info!("{said}"),
            Err(e) => warn!("cannot make the change in {}: {e}", path.display()),
        }
    }
}

/// Changes whom a document is shared with, as an admin asked: everyone in its room hears the
/// new access, new members are sent the link, and it moves to a fresh room when someone was
/// removed without link sharing, or link sharing was turned off. What was done, for the log.
fn change_sharing(world: &mut World, c: &admin::SharingChange) -> Result<String, String> {
    let mut docs = world.resource_mut::<Documents>();
    let docs = &mut *docs;
    let (name, said, told) = if let Some(h) = docs.open.get_mut(&c.document) {
        let sharing = h.meta.sharing.as_mut().ok_or("It isn't shared")?;
        let (rotate, said) = admin::share(sharing, &docs.users, &c.what)?;
        // The old room hears it before the document leaves it.
        let told = h.access_changed(c.document).map(|m| (h.room, m));
        if rotate && let Some(s) = h.meta.sharing.as_mut() {
            s.rotate(random_topic());
        }
        docs.store
            .save(&h.confirmed, &h.meta)
            .map_err(|e| e.to_string())?;
        (h.name.clone(), said, told)
    } else {
        let mut file = docs.store.load(c.document).map_err(|e| e.to_string())?;
        if file.meta.shared.is_some() {
            return Err("It is someone else's document".into());
        }
        let sharing = file
            .meta
            .sharing
            .get_or_insert_with(|| cadrs_core::collab::Sharing::new(random_topic()));
        let (rotate, said) = admin::share(sharing, &docs.users, &c.what)?;
        if rotate {
            sharing.rotate(random_topic());
        }
        docs.store
            .save(&file.document, &file.meta)
            .map_err(|e| e.to_string())?;
        (file.document.name, said, None)
    };
    docs.next_invites = 0.0;
    docs.rescan = true;
    world.resource_mut::<Outbox>().0.extend(told);
    Ok(format!("{name}: {}", said.join(", ")))
}

/// A fresh room's topic (as the app makes them).
fn random_topic() -> [u8; 32] {
    let mut t = [0; 32];
    t[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    t[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    t
}

/// Takes the users as `users.ron` has them now (an admin may have changed them); keeps the
/// ones it has when the file can't be read.
fn read_users(docs: &mut Documents) {
    match Users::read(docs.store.root()).and_then(|u| u.check().map(|_| u)) {
        Ok(users) => {
            if users != docs.users {
                info!("users changed: {} users now", users.users.len());
                docs.users = users;
            }
            docs.users_error = None;
        }
        Err(e) => {
            if docs.users_error.as_ref() != Some(&e) {
                error!("{e}; keeping the users as they were");
                docs.users_error = Some(e);
            }
        }
    }
}

/// Saves a document if it has edits not saved yet, and leaves its room.
fn close(world: &mut World, id: DocumentId, store: &Store) {
    let Some(h) = world.resource_mut::<Documents>().open.remove(&id) else {
        return;
    };
    if h.unsaved.is_some() {
        if let Err(e) = store.save(&h.confirmed, &h.meta) {
            error!("cannot save {}: {e}", h.name);
        }
        if let Err(e) = h.log.save(store) {
            error!("cannot save the history of {}: {e}", h.name);
        }
    }
    if let Ok(e) = world.get_entity_mut(h.room) {
        e.despawn();
    }
    info!("no longer hosting {}", h.name);
}

/// A peer's name as their cadrs says it, or a short form of their key.
fn peer_name(world: &mut World, id: EndpointId) -> String {
    let mut q = world.query::<&Peer>();
    q.iter(world)
        .find(|p| p.id == id)
        .map(Peer::label)
        .unwrap_or_else(|| id.fmt_short().to_string())
}

/// Answers the peers' requests.
fn serve(world: &mut World) {
    let Some(requests) = world
        .get_resource_mut::<Requests>()
        .map(|mut r| r.drain())
        .filter(|r| !r.is_empty())
    else {
        return;
    };
    for r in requests {
        let Some(r) = refuse_strangers(world, r) else {
            continue;
        };
        match r.ask {
            Ask::Join => join(world, r),
            Ask::Propose { .. } => propose(world, r),
            Ask::Comment(_) => comment(world, r),
            Ask::CreateVersion { .. } => version(world, r),
            Ask::Lock { .. } | Ask::Unlock { .. } => lock(world, r),
            Ask::AddMember { .. } | Ask::Leave => membership(world, r),
            Ask::Handover(_) => handover(world, r),
            Ask::Invite(_) => invited(world, r),
            Ask::TakeOwnership(_) => take_ownership(world, r),
            Ask::Status => {
                let open = hosted(world, &r).is_ok();
                r.verdict(Verdict::Accepted(open as u64))
            }
        }
    }
}

/// What the host says to cadrs IDs that aren't its users'.
const STRANGER: &str = "This host doesn't know your cadrs ID: ask its admin to add it";

/// The request, if it comes from one of the host's users; otherwise refused, whatever it asks.
fn refuse_strangers(world: &World, r: Request) -> Option<Request> {
    let from = r.from.to_string();
    if world.resource::<Documents>().users.by_id(&from).is_some() {
        return Some(r);
    }
    info!(
        "refused {} of {} to {}: not one of the host's users",
        asked(&r.ask),
        r.document,
        r.from.fmt_short()
    );
    match r.ask {
        Ask::Join | Ask::Status => r.refuse(STRANGER),
        _ => r.verdict(Verdict::Refused(STRANGER.into())),
    }
    None
}

/// What a request asks, for the log.
fn asked(ask: &Ask) -> &'static str {
    match ask {
        Ask::Join => "Join",
        Ask::Propose { .. } => "Propose",
        Ask::Invite(_) => "Invite",
        Ask::AddMember { .. } => "AddMember",
        Ask::Leave => "Leave",
        Ask::Comment(_) => "Comment",
        Ask::CreateVersion { .. } => "CreateVersion",
        Ask::Lock { .. } => "Lock",
        Ask::Unlock { .. } => "Unlock",
        Ask::Handover(_) => "Handover",
        Ask::TakeOwnership(_) => "TakeOwnership",
        Ask::Status => "Status",
    }
}

/// The document a request names, if the host has it open in the room the request gave; `Err`
/// is the refusal.
fn hosted<'a>(world: &'a World, r: &Request) -> Result<&'a Hosted, String> {
    match world.resource::<Documents>().open.get(&r.document) {
        Some(h) if h.topic == r.topic => Ok(h),
        Some(_) => Err("This document is no longer shared".into()),
        None => Err("The host does not have this document".into()),
    }
}

fn join(world: &mut World, r: Request) {
    let docs = world.resource::<Documents>();
    let answer = hosted(world, &r).and_then(|h| {
        let access = h
            .meta
            .sharing
            .as_ref()
            .and_then(|s| s.access(&r.from.to_string()))
            .ok_or("Link sharing is off for this document")?;
        let snapshot = Snapshot::of(&h.confirmed, h.seq.seq(), &docs.store)?;
        Ok(Welcome {
            comments: Comments::load(&docs.store.doc_dir(r.document)),
            access,
            owner_name: docs.name.clone(),
            epoch: h.epoch,
            snapshot,
        })
    });
    match &answer {
        Ok(_) => info!("sent {} to {}", r.document, r.from.fmt_short()),
        Err(why) => info!("refused {} to {}: {why}", r.document, r.from.fmt_short()),
    }
    r.welcome(answer);
}

fn propose(world: &mut World, r: Request) {
    let Ask::Propose {
        epoch,
        id,
        base,
        change,
    } = &r.ask
    else {
        return;
    };
    let (epoch, id, base) = (*epoch, *id, *base);
    let author = r.from.to_string();
    let permission = match hosted(world, &r) {
        Ok(h) => h.meta.sharing.as_ref().and_then(|s| s.permission(&author)),
        Err(why) => return r.refuse(why),
    };
    if permission != Some(Permission::Edit) {
        return r.verdict(Verdict::Refused(
            "You have view access to this document".into(),
        ));
    }
    let author_name = peer_name(world, r.from);
    let t = now(world);
    let mut docs = world.resource_mut::<Documents>();
    let Some(h) = docs.open.get_mut(&r.document) else {
        return r.verdict(Verdict::Closed);
    };
    if h.epoch != epoch {
        return r.verdict(Verdict::Resync);
    }
    // A tab someone else has open for themselves.
    if let Some(name) = change
        .footprint()
        .0
        .iter()
        .find_map(|k| h.held_against(*k, &author))
    {
        return r.verdict(Verdict::Refused(format!(
            "{name} has that tab open: your change was undone"
        )));
    }
    let verdict = match h.number(
        r.document,
        (&author, &author_name),
        id,
        base,
        change.clone(),
        t,
    ) {
        Ok((seq, m)) => {
            let room = h.room;
            world.resource_mut::<Outbox>().0.push((room, m));
            Verdict::Accepted(seq)
        }
        Err(Refusal::Conflict { author: first, .. }) => {
            let who = match first.parse::<EndpointId>() {
                Ok(id) => peer_name(world, id),
                Err(_) => first,
            };
            Verdict::Refused(format!("{who} changed that first: your change was undone"))
        }
        Err(Refusal::Stale) => Verdict::Resync,
    };
    r.verdict(verdict);
}

/// A room that once had a member's link but no longer does: the host sends everyone the link
/// again, as an update (the value the app uses for the same).
const OLD_ROOM: [u8; 32] = [0; 32];

/// The request, if it comes from a maintainer or an admin; otherwise refused.
fn refuse_unless_maintainer(world: &World, r: Request) -> Option<Request> {
    let from = r.from.to_string();
    let role = world.resource::<Documents>().users.role(&from);
    if role >= Some(Role::Maintainer) {
        return Some(r);
    }
    info!(
        "refused {} of {} from {}: a {}",
        asked(&r.ask),
        r.document,
        r.from.fmt_short(),
        role.unwrap_or(Role::Collaborator)
    );
    r.verdict(Verdict::Refused(
        "This host takes documents only from its maintainers and admins".into(),
    ));
    None
}

/// A document shared with the host: a maintainer's, about to be given to it (Transfer
/// ownership needs the host among its members). Taken, but not kept: the host keeps only what
/// it owns.
fn invited(world: &mut World, r: Request) {
    if let Some(r) = refuse_unless_maintainer(world, r) {
        r.verdict(Verdict::Accepted(0));
    }
}

/// **Transfer ownership** from a maintainer or an admin: the document, who it is shared with
/// and its comments are the host's now, and they maintain it. The old owner stays a member,
/// with Can edit, and everyone it is shared with is sent the new link.
fn take_ownership(world: &mut World, r: Request) {
    let Some(r) = refuse_unless_maintainer(world, r) else {
        return;
    };
    let Ask::TakeOwnership(o) = &r.ask else {
        return;
    };
    let o = (**o).clone();
    if world.resource::<Documents>().open.contains_key(&r.document) {
        return r.verdict(Verdict::Refused(
            "The host has this document already".into(),
        ));
    }
    let doc = match o.snapshot.clone().install() {
        Ok(d) if d.id == r.document => d,
        _ => return r.verdict(Verdict::Refused("Not this document".into())),
    };
    let me = own_id(world);
    let from = r.from.to_string();
    let mut sharing = o.sharing.clone();
    sharing.members.retain(|m| m.id != me && m.id != from);
    for m in &mut sharing.members {
        if m.delivered.is_some() {
            m.delivered = Some(OLD_ROOM);
        }
    }
    sharing.members.push(Member {
        id: from,
        name: o.from_name.clone(),
        permission: Permission::Edit,
        rights: None,
        delivered: Some(OLD_ROOM),
    });
    let mut docs = world.resource_mut::<Documents>();
    let mut meta = DocumentMeta::new(&docs.name, unix_now());
    meta.sharing = Some(sharing);
    let kept = docs
        .store
        .create(&doc, &meta)
        .map_err(|e| e.to_string())
        .and_then(|_| {
            o.comments
                .save(&docs.store.doc_dir(doc.id))
                .map_err(|e| e.to_string())
        });
    if let Err(e) = kept {
        error!("cannot keep {}: {e}", doc.name);
        return r.verdict(Verdict::Refused(format!("The host cannot keep it: {e}")));
    }
    let giver = docs
        .users
        .by_id(&r.from.to_string())
        .map(|u| u.name.clone());
    let users = giver
        .as_deref()
        .map(|giver| Users::edit(docs.store.root(), |u| u.set_maintainer(doc.id, giver, true)));
    match users {
        Some(Ok((users, ()))) => docs.users = users,
        Some(Err(e)) => error!("cannot note who maintains {}: {e}", doc.name),
        None => {}
    }
    let unknown: Vec<&str> = meta
        .sharing
        .iter()
        .flat_map(|s| &s.members)
        .filter(|m| docs.users.by_id(&m.id).is_none())
        .map(|m| m.name.as_str())
        .collect();
    if !unknown.is_empty() {
        warn!(
            "{} is shared with people who aren't users of the host, who can't open it: {}",
            doc.name,
            unknown.join(", ")
        );
    }
    info!(
        "{} gave the host {}",
        giver.as_deref().unwrap_or(&o.from_name),
        doc.name
    );
    docs.rescan = true;
    docs.next_invites = 0.0;
    r.verdict(Verdict::Accepted(0));
}

/// From the editor who kept the edits in order while the host was away (down, or not started
/// yet): the document as it is now, taken on as one edit by them, as the owner's app does
/// when it is back. Everyone then follows the host again.
fn handover(world: &mut World, r: Request) {
    let Ask::Handover(snapshot) = &r.ask else {
        return;
    };
    let author = r.from.to_string();
    match hosted(world, &r) {
        Ok(h)
            if h.meta.sharing.as_ref().and_then(|s| s.permission(&author))
                != Some(Permission::Edit) =>
        {
            return r.verdict(Verdict::Refused("Not taken".into()));
        }
        Ok(_) => {}
        Err(why) => return r.refuse(why),
    }
    let doc = match snapshot.clone().install() {
        Ok(d) if d.id == r.document => d,
        _ => return r.verdict(Verdict::Refused("Not this document".into())),
    };
    let author_name = peer_name(world, r.from);
    let t = now(world);
    let mut docs = world.resource_mut::<Documents>();
    let Some(h) = docs.open.get_mut(&r.document) else {
        return r.verdict(Verdict::Closed);
    };
    if *h.confirmed == doc {
        return r.verdict(Verdict::Accepted(0));
    }
    let change = Change::between(&h.confirmed, &doc);
    let base = h.seq.seq();
    match h.number(r.document, (&author, &author_name), 0, base, change, t) {
        Ok((_, m)) => {
            *h.confirmed = doc;
            info!("took {} back from {author_name}", h.name);
            let room = h.room;
            world.resource_mut::<Outbox>().0.push((room, m));
            r.verdict(Verdict::Accepted(0));
        }
        Err(_) => r.verdict(Verdict::Refused("Not taken".into())),
    }
}

/// A comment from someone the document is shared with, kept next to it and passed on to the
/// room.
fn comment(world: &mut World, r: Request) {
    let Ask::Comment(op) = &r.ask else { return };
    let from = r.from.to_string();
    let room = match hosted(world, &r) {
        Ok(h) => {
            let may = h
                .meta
                .sharing
                .as_ref()
                .and_then(|s| s.access(&from))
                .is_some_and(|a| a.rights.comment);
            if !may {
                return r.verdict(Verdict::Refused(
                    "You may not comment on this document".into(),
                ));
            }
            h.room
        }
        Err(why) => return r.refuse(why),
    };
    // (Each comment is its author's: one in someone else's name is not taken.)
    if let CommentOp::Add(c) = op
        && c.author != from
    {
        return r.verdict(Verdict::Refused(
            "A comment can only be added in one's own name".into(),
        ));
    }
    let me = world
        .get_resource::<Iroh>()
        .and_then(|i| cadrs_net::cadrs_id(Some(i)))
        .unwrap_or_default();
    let dir = world.resource::<Documents>().store.doc_dir(r.document);
    let mut cs = Comments::load(&dir);
    let result = cs
        .apply(op, &from, &me)
        .map_err(|e| e.to_string())
        .and_then(|()| cs.save(&dir).map_err(|e| e.to_string()));
    match result {
        Ok(()) => {
            if let Some(m) = CommentChanged::new(r.document, &from, op) {
                world
                    .resource_mut::<Outbox>()
                    .0
                    .push((room, Outgoing::Comment(m)));
            }
            r.verdict(Verdict::Accepted(0));
        }
        Err(why) => {
            info!("comment from {} not applied: {why}", r.from.fmt_short());
            r.verdict(Verdict::Refused(why));
        }
    }
}

/// **Create version**, asked by someone who may edit: made of the document as everyone has
/// it (after the last numbered edit), and told to the room, where everyone makes the same
/// version of their copy at that edit.
fn version(world: &mut World, r: Request) {
    let Ask::CreateVersion {
        epoch,
        name,
        description,
    } = &r.ask
    else {
        return;
    };
    let from = r.from.to_string();
    match hosted(world, &r) {
        Ok(h) if h.epoch != *epoch => return r.verdict(Verdict::Resync),
        Ok(h)
            if h.meta.sharing.as_ref().and_then(|s| s.permission(&from))
                != Some(Permission::Edit) =>
        {
            return r.verdict(Verdict::Refused(
                "You have view access to this document".into(),
            ));
        }
        Ok(_) => {}
        Err(why) => return r.refuse(why),
    }
    let user = peer_name(world, r.from);
    let time = unix_now();
    let mut docs = world.resource_mut::<Documents>();
    let docs = &mut *docs;
    let Some(h) = docs.open.get_mut(&r.document) else {
        return r.verdict(Verdict::Closed);
    };
    let id = h
        .log
        .create_version_with_id(VersionId::new(), name, description, time, &user);
    let made = h
        .log
        .versions()
        .iter()
        .find(|v| v.id() == id)
        .map(|v| v.name().to_string())
        .unwrap_or_default();
    if let Err(e) = h.log.save(&docs.store) {
        error!("cannot save the history of {}: {e}", h.name);
        return r.verdict(Verdict::Refused(format!("The host could not save it: {e}")));
    }
    info!("{user} made version {made} of {}", h.name);
    let m = VersionCreated {
        document: r.document,
        epoch: h.epoch,
        seq: h.seq.seq(),
        id: id.0,
        name: made,
        description: description.clone(),
        time,
        user,
    };
    let room = h.room;
    world
        .resource_mut::<Outbox>()
        .0
        .push((room, Outgoing::Version(m)));
    r.verdict(Verdict::Accepted(0));
}

/// Where each peer is, from their presence; and the locks again for whoever joins a room (a
/// lock taken before they came would otherwise reach them only when it changes).
fn hear(
    mut presence: MessageReader<Received<Presence>>,
    mut joined: MessageReader<PeerJoined>,
    time: Res<Time<bevy::time::Real>>,
    mut docs: ResMut<Documents>,
    mut outbox: ResMut<Outbox>,
) {
    let t = time.elapsed_secs_f64();
    for m in presence.read() {
        if let Some(h) = docs
            .open
            .get_mut(&m.msg.document)
            .filter(|h| h.room == m.room)
        {
            h.present.insert(m.from.to_string(), (m.msg.tab, t));
        }
    }
    for j in joined.read() {
        if let Some((id, h)) = docs
            .open
            .iter()
            .find(|(_, h)| h.room == j.room && !h.locks.is_empty())
        {
            outbox.0.push((h.room, h.locks_changed(*id)));
        }
    }
    for h in docs.open.values_mut() {
        h.present.retain(|_, (_, heard)| t - *heard < FORGET_AFTER);
    }
}

/// **Open tab, close it for other users**, and giving it back, from someone who may edit.
/// Taking a tab someone else holds needs `take` (**Take over**); only its holder gives it back.
fn lock(world: &mut World, r: Request) {
    let from = r.from.to_string();
    match hosted(world, &r) {
        Ok(h)
            if h.meta.sharing.as_ref().and_then(|s| s.permission(&from))
                != Some(Permission::Edit) =>
        {
            return r.verdict(Verdict::Refused(
                "You have view access to this document".into(),
            ));
        }
        Ok(_) => {}
        Err(why) => return r.refuse(why),
    }
    let name = peer_name(world, r.from);
    let t = now(world);
    let mut docs = world.resource_mut::<Documents>();
    let Some(h) = docs.open.get_mut(&r.document) else {
        return r.verdict(Verdict::Closed);
    };
    let (element, holder, take) = match r.ask {
        Ask::Lock { element, take } => (element, Some((from.clone(), name)), take),
        Ask::Unlock { element } => (element, None, false),
        _ => return,
    };
    let current = h.locks.iter().position(|(e, ..)| *e == element);
    match (holder, current) {
        (Some((id, name)), Some(i)) => {
            let (_, held_by, held_name) = &h.locks[i];
            if *held_by != id && !take {
                let why = format!("{held_name} has this tab open");
                return r.verdict(Verdict::Refused(why));
            }
            h.locks[i] = (element, id, name);
        }
        (Some((id, name)), None) => h.locks.push((element, id, name)),
        // Only its holder gives a tab back; anyone else's Unlock changes nothing.
        (None, Some(i)) if h.locks[i].1 == from => {
            h.locks.remove(i);
        }
        (None, _) => return r.verdict(Verdict::Accepted(0)),
    }
    h.away.retain(|(e, _)| *e != element);
    // (Just asked for: the holder is in that tab, whatever their last presence said.)
    if h.locks
        .iter()
        .any(|(e, id, _)| *e == element && *id == from)
    {
        h.present.insert(from, (Some(element), t));
    }
    let (room, m) = (h.room, h.locks_changed(r.document));
    world.resource_mut::<Outbox>().0.push((room, m));
    r.verdict(Verdict::Accepted(0));
}

/// The host's own cadrs ID.
fn own_id(world: &World) -> String {
    cadrs_net::cadrs_id(world.get_resource::<Iroh>()).unwrap_or_default()
}

/// Someone who may share (the **Share** checkbox) asks the host to share the document with
/// someone else, giving no more than they have; or a member leaves it (**Unshare with me**).
/// Everyone in the room hears the new access, and a new member is sent the link.
fn membership(world: &mut World, r: Request) {
    let from = r.from.to_string();
    let users = &world.resource::<Documents>().users;
    // A maintainer of the document, or an admin, manages it whatever their own access.
    let manager = users.maintains(&from, r.document).then(|| Access {
        permission: Permission::Edit,
        rights: Rights::of(Permission::Edit),
    });
    let stranger = match &r.ask {
        Ask::AddMember { id, name, .. } if users.by_id(id).is_none() => Some(name.clone()),
        _ => None,
    };
    let mine = match hosted(world, &r) {
        Ok(h) => manager.or_else(|| h.meta.sharing.as_ref().and_then(|s| s.access(&from))),
        Err(why) => return r.refuse(why),
    };
    let me = own_id(world);
    let asker = peer_name(world, r.from);
    let mut docs = world.resource_mut::<Documents>();
    let docs = &mut *docs;
    let Some(h) = docs.open.get_mut(&r.document) else {
        return r.verdict(Verdict::Closed);
    };
    let Some(sharing) = h.meta.sharing.as_mut() else {
        return r.verdict(Verdict::Closed);
    };
    let what = match &r.ask {
        Ask::AddMember { id, name, access } => {
            let Some(mine) = mine.filter(|a| a.rights.share) else {
                return r.verdict(Verdict::Refused(
                    "You may not share this document: ask its owner".into(),
                ));
            };
            if *id == me {
                return r.verdict(Verdict::Refused("That is the host's own cadrs ID".into()));
            }
            if id.parse::<EndpointId>().is_err() {
                let why = format!("{id} is not a cadrs ID");
                return r.verdict(Verdict::Refused(why));
            }
            if let Some(who) = stranger {
                let who = if who.is_empty() { id.clone() } else { who };
                let why = format!(
                    "{who} isn't one of the host's users, who can't open it: ask its admin to add them"
                );
                return r.verdict(Verdict::Refused(why));
            }
            let permission = if mine.permission.can_edit() {
                access.permission
            } else {
                Permission::View
            };
            match sharing.members.iter_mut().find(|m| m.id == *id) {
                Some(m) => {
                    m.permission = permission;
                    m.rights = Some(access.rights);
                    if !name.is_empty() {
                        m.name = name.clone();
                    }
                }
                None => sharing.members.push(Member {
                    id: id.clone(),
                    name: name.clone(),
                    permission,
                    rights: Some(access.rights),
                    delivered: None,
                }),
            }
            docs.next_invites = 0.0;
            format!("{asker} shared {} with {name}", h.name)
        }
        Ask::Leave => {
            sharing.members.retain(|m| m.id != from);
            h.invites.remove(&from);
            format!("{asker} left {}", h.name)
        }
        _ => return,
    };
    if let Err(e) = docs.store.save(&h.confirmed, &h.meta) {
        error!("cannot save {}: {e}", h.name);
        return r.verdict(Verdict::Refused(format!("The host could not save it: {e}")));
    }
    info!("{what}");
    let m = h.access_changed(r.document);
    let room = h.room;
    world
        .resource_mut::<Outbox>()
        .0
        .extend(m.map(|m| (room, m)));
    r.verdict(Verdict::Accepted(0));
}

/// Sends each document's link to every member whose cadrs doesn't have the current one, and
/// notes who took it.
fn deliver_invites(world: &mut World) {
    let t = now(world);
    // The answers.
    let mut docs = world.resource_mut::<Documents>();
    let docs = &mut *docs;
    for h in docs.open.values_mut() {
        let mut taken = Vec::new();
        h.invites
            .retain(|member, (topic, task)| match task.try_take() {
                Some(Ok(Verdict::Accepted(_))) => {
                    taken.push((member.clone(), *topic));
                    false
                }
                Some(Ok(other)) => {
                    info!("{member} did not take the link to {}: {other:?}", h.name);
                    false
                }
                Some(Err(why)) => {
                    debug!("cannot reach {member} yet: {why}");
                    false
                }
                None => true,
            });
        let Some(sharing) = h.meta.sharing.as_mut().filter(|_| !taken.is_empty()) else {
            continue;
        };
        for (member, topic) in taken {
            if let Some(m) = sharing.members.iter_mut().find(|m| m.id == member) {
                m.delivered = Some(topic);
            }
        }
        if let Err(e) = docs.store.save(&h.confirmed, &h.meta) {
            warn!("cannot note the links sent for {}: {e}", h.name);
        }
    }
    if t < docs.next_invites {
        return;
    }
    docs.next_invites = t + RETRY_INVITES;
    // The links still to send.
    let Some(owner) = world.get_resource::<Iroh>().and_then(Iroh::addr) else {
        return;
    };
    let docs = world.resource::<Documents>();
    let mut sends = Vec::new();
    for (id, h) in &docs.open {
        let Some(sharing) = &h.meta.sharing else {
            continue;
        };
        let link = ShareLink {
            document: *id,
            name: h.confirmed.name.clone(),
            topic: sharing.topic,
            owner: owner.clone(),
        };
        for m in &sharing.members {
            if m.delivered == Some(sharing.topic) || h.invites.contains_key(&m.id) {
                continue;
            }
            // Someone the host doesn't answer would only be refused.
            if docs.users.by_id(&m.id).is_none() {
                continue;
            }
            let Ok(to) = m.id.parse::<EndpointId>() else {
                continue;
            };
            let invite = Invite {
                link: link.to_string(),
                owner_name: docs.name.clone(),
                access: m.access(),
                message: String::new(),
                update: m.delivered.is_some(),
            };
            sends.push((*id, m.id.clone(), to, link.clone(), invite));
        }
    }
    for (id, member, to, link, invite) in sends {
        let task = transfer::start_invite(world.resource::<Iroh>(), to, &link, invite);
        if let Some(h) = world.resource_mut::<Documents>().open.get_mut(&id) {
            h.invites.insert(member, (link.topic, task));
        }
    }
}

/// Lets a lock go when its holder has been away from its tab (or the document) for a while.
fn release_left(
    time: Res<Time<bevy::time::Real>>,
    mut docs: ResMut<Documents>,
    mut outbox: ResMut<Outbox>,
) {
    let t = time.elapsed_secs_f64();
    for (id, h) in docs.open.iter_mut() {
        if h.locks.is_empty() {
            continue;
        }
        let mut gone = Vec::new();
        for (element, holder, _) in &h.locks {
            let there = h
                .present
                .get(holder)
                .is_some_and(|(tab, _)| *tab == Some(*element));
            if there {
                h.away.retain(|(e, _)| e != element);
                continue;
            }
            let since = match h.away.iter().find(|(e, _)| e == element) {
                Some((_, s)) => *s,
                None => {
                    h.away.push((*element, t));
                    t
                }
            };
            if t - since > AWAY {
                gone.push(*element);
            }
        }
        if gone.is_empty() {
            continue;
        }
        h.locks.retain(|(e, ..)| !gone.contains(e));
        h.away.retain(|(e, _)| !gone.contains(e));
        info!(
            "let {} tab(s) of {} go: their holders left",
            gone.len(),
            h.name
        );
        outbox.0.push((h.room, h.locks_changed(*id)));
    }
}

fn send(mut outbox: ResMut<Outbox>, net: NetSender) {
    for (room, m) in outbox.0.drain(..) {
        match m {
            Outgoing::Edit(m) => {
                net.broadcast(room, &m);
            }
            Outgoing::Version(m) => {
                net.broadcast(room, &m);
            }
            Outgoing::Comment(m) => {
                net.broadcast(room, &m);
            }
            Outgoing::Locks(m) => {
                net.broadcast(room, &m);
            }
            Outgoing::Access(m) => {
                net.broadcast(room, &m);
            }
        }
    }
}

/// Saves the documents whose edits have waited long enough.
/// On the way out (Ctrl+C, which `docker stop` sends with the image's `STOPSIGNAL`), saves
/// every edit not saved yet and the history logs, so a restart loses nothing.
fn save_on_exit(exit: MessageReader<AppExit>, mut docs: ResMut<Documents>) {
    if exit.is_empty() {
        return;
    }
    let docs = &mut *docs;
    for h in docs.open.values_mut() {
        if h.unsaved.take().is_none() {
            continue;
        }
        h.meta.modified = unix_now();
        if let Err(e) = docs.store.save(&h.confirmed, &h.meta) {
            error!("cannot save {}: {e}", h.name);
        }
        if let Err(e) = h.log.save(&docs.store) {
            error!("cannot save the history of {}: {e}", h.name);
        }
    }
    // (And the history saves still on their way.)
    cadrs_core::history_log::flush_saves();
    info!("saved everything: stopping");
}

fn save(world: &mut World) {
    let t = now(world);
    let mut docs = world.resource_mut::<Documents>();
    let docs = &mut *docs;
    for h in docs.open.values_mut() {
        if h.unsaved.is_none_or(|since| t - since < SAVE_AFTER) {
            continue;
        }
        h.meta.modified = unix_now();
        match docs.store.save(&h.confirmed, &h.meta) {
            Ok(()) => {
                h.unsaved = None;
                h.log.save_in_background(&docs.store);
            }
            Err(e) => {
                error!("cannot save {}: {e}", h.name);
                // Try again in a while, not every frame.
                h.unsaved = Some(t);
            }
        }
    }
}

/// Says now and then what the host keeps.
fn report(world: &mut World, mut last: Local<Option<f64>>) {
    let t = now(world);
    if last.is_some_and(|l| t - l < REPORT_EVERY) {
        return;
    }
    *last = Some(t);
    let n = world.resource::<Documents>().open.len();
    info!("host: {n} documents in their rooms");
}
