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
//! This first step answers Join, Propose and Status. Comments, versions, locks, sharing
//! requests, handover and transfers are refused for now, and the history log is not written
//! yet: each numbered edit is saved as the document itself.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bevy::app::ScheduleRunnerPlugin;
use bevy::prelude::*;
use cadrs_core::collab::comments::Comments;
use cadrs_core::collab::sync::{Refusal, Sequencer};
use cadrs_core::collab::{Permission, Snapshot};
use cadrs_core::history_log::document_hash;
use cadrs_core::library::DocumentMeta;
use cadrs_core::{Document, DocumentId, Store};
use cadrs_net::bevy_iroh::iroh::EndpointId;
use cadrs_net::bevy_iroh::prelude::{NetSender, Peer};
use cadrs_net::bevy_iroh::{Iroh, Relays};
use cadrs_net::{Ask, Request, Requests, Sequenced, Verdict, Welcome, session};

/// How often the data dir is read again for documents that came, went or changed rooms.
const SCAN_EVERY: f64 = 5.0;
/// How long a document waits after an edit before it is saved (a drag sends several a second).
const SAVE_AFTER: f64 = 1.0;
/// How often the host says how it is doing.
const REPORT_EVERY: f64 = 300.0;

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
    meta: DocumentMeta,
    /// When the first edit not saved yet was numbered.
    unsaved: Option<f64>,
}

/// The documents the host owns, by id.
#[derive(Resource)]
struct Documents {
    store: Store,
    /// What collaborators see as the owner's name.
    name: String,
    open: HashMap<DocumentId, Hosted>,
}

/// Numbered edits on their way to their rooms.
#[derive(Resource, Default)]
struct Outbox(Vec<(Entity, Sequenced)>);

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
    })
    .init_resource::<Outbox>()
    .add_systems(Update, (scan, serve, send, save, report).chain());
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
                meta: file.meta,
                unsaved: None,
            },
        );
    }
}

/// Saves a document if it has edits not saved yet, and leaves its room.
fn close(world: &mut World, id: DocumentId, store: &Store) {
    let Some(h) = world.resource_mut::<Documents>().open.remove(&id) else {
        return;
    };
    if h.unsaved.is_some()
        && let Err(e) = store.save(&h.confirmed, &h.meta)
    {
        error!("cannot save {}: {e}", h.name);
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
            h.unsaved.get_or_insert(t);
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

fn send(mut outbox: ResMut<Outbox>, net: NetSender) {
    for (room, m) in outbox.0.drain(..) {
        net.broadcast(room, &m);
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
        h.meta.modified = cadrs_net::public::unix_now() as i64;
        match docs.store.save(&h.confirmed, &h.meta) {
            Ok(()) => h.unsaved = None,
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
