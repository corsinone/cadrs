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
//! answers Join, Propose, Status, Comment, CreateVersion, Lock, Unlock, AddMember and Leave;
//! handover and transfers are refused for now.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bevy::app::ScheduleRunnerPlugin;
use bevy::prelude::*;
use cadrs_core::collab::comments::{CommentOp, Comments};
use cadrs_core::collab::sync::{Key, Refusal, Sequencer};
use cadrs_core::collab::{Member, Permission, Snapshot};
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
}

/// `Some` when the arguments ask for a host.
pub fn parse(args: &[String]) -> Option<Result<Host, String>> {
    if !args.iter().any(|a| a == "--host") {
        return None;
    }
    let mut dir = std::env::var_os("CADRS_DATA_DIR").map(PathBuf::from);
    let mut name = "cadrs host".to_string();
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
            other => {
                return Some(Err(format!(
                    "{other}: a host takes only --data-dir and --name"
                )));
            }
        }
    }
    let dir = dir.or_else(|| {
        Store::default_root().map(|r| r.parent().map_or(r.clone(), |p| p.join("host")))
    });
    Some(match dir {
        Some(dir) => Ok(Host { dir, name }),
        None => Err("No data dir: give one with --data-dir".into()),
    })
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
    if let Err(e) = std::fs::create_dir_all(&host.dir) {
        eprintln!("cannot use {}: {e}", host.dir.display());
        return AppExit::from_code(2);
    }
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(Duration::from_secs_f64(
            1.0 / 30.0,
        ))),
        bevy::log::LogPlugin::default(),
    ));
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
    app.insert_resource(Documents {
        store: Store::new(dir),
        name: name.into(),
        open: HashMap::new(),
        next_invites: 0.0,
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
    );
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
    if last.is_some_and(|l| t - l < SCAN_EVERY) {
        return;
    }
    *last = Some(t);
    let store = world.resource::<Documents>().store.clone();
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
        match r.ask {
            Ask::Join => join(world, r),
            Ask::Propose { .. } => propose(world, r),
            Ask::Comment(_) => comment(world, r),
            Ask::CreateVersion { .. } => version(world, r),
            Ask::Lock { .. } | Ask::Unlock { .. } => lock(world, r),
            Ask::AddMember { .. } | Ask::Leave => membership(world, r),
            Ask::Status => {
                let open = hosted(world, &r).is_ok();
                r.verdict(Verdict::Accepted(open as u64))
            }
            _ => r.refuse("This host does not do that yet"),
        }
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
    let verdict = match h.seq.admit(&author, base, change.clone(), &mut h.confirmed) {
        Ok((seq, change)) => {
            let m = Sequenced::new(
                r.document,
                h.epoch,
                seq,
                (&author, &author_name),
                id,
                &change,
                document_hash(&h.confirmed),
            );
            h.log.record(
                &h.confirmed,
                Origin::Command(String::new()),
                unix_now(),
                &author_name,
            );
            h.unsaved.get_or_insert(t);
            let room = h.room;
            world
                .resource_mut::<Outbox>()
                .0
                .push((room, Outgoing::Edit(m)));
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
    let mine = match hosted(world, &r) {
        Ok(h) => h.meta.sharing.as_ref().and_then(|s| s.access(&from)),
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
