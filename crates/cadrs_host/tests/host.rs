//! A host and its peers in one process, over real iroh with no relay: the host owns a shared
//! document in its data dir, an editor fetches it and edits it, a viewer and a stranger are
//! kept to what they may do, the editor makes a version and the viewer comments, and all of it
//! is on disk afterwards: the document, its history log and its comments. Then two editors and
//! a tab one of them has open for themselves; members added, sent the link, and leaving; a
//! document handed back after the host was away; one a maintainer gives the host; the public
//! room, which the host stays out of unless it is turned on; a restart; the host's users, the
//! only ones it answers, and the commands that change them.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bevy::prelude::*;
use cadrs_core::collab::comments::{Comment, CommentOp, Comments};
use cadrs_core::collab::sync::Change;
use cadrs_core::collab::{Access, Member, Permission, Rights, Sharing, Snapshot};
use cadrs_core::history_log::HistoryLog;
use cadrs_core::library::DocumentMeta;
use cadrs_core::{Document, Store};
use cadrs_host::users::{Role, User, Users};
use cadrs_net::bevy_iroh::{Iroh, IrohTask, Relays};
use cadrs_net::{Ask, NetPlugin, Ownership, Requests, ShareLink, Verdict, Welcome, transfer};

const PATIENCE: Duration = Duration::from_secs(30);
const TOPIC: [u8; 32] = [7; 32];

fn peer(name: &str) -> App {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        NetPlugin {
            identity: None,
            display_name: name.into(),
            relays: Relays::Disabled,
            autostart: true,
        },
    ));
    app
}

fn id_of(app: &App) -> String {
    app.world().resource::<Iroh>().id().to_string()
}

fn member(app: &App, name: &str, permission: Permission) -> Member {
    Member {
        id: id_of(app),
        name: name.into(),
        permission,
        rights: None,
        delivered: None,
    }
}

/// Makes `app`'s owner a user of the host in `dir`, called `name`.
fn user(dir: &Path, app: &App, name: &str, role: Role) {
    let id = id_of(app);
    Users::edit(dir, |u| {
        u.users.push(User {
            name: name.into(),
            role,
            sign_in: None,
            ids: vec![id],
        });
        Ok(())
    })
    .unwrap();
}

fn wait<T>(apps: &mut [&mut App], mut task: IrohTask<T>) -> T {
    let start = Instant::now();
    loop {
        for a in apps.iter_mut() {
            a.update();
        }
        if let Some(r) = task.try_take() {
            return r;
        }
        assert!(start.elapsed() < PATIENCE, "gave up waiting for the host");
        std::thread::sleep(Duration::from_millis(16));
    }
}

fn fetch(host: &mut App, peer: &mut App, link: &ShareLink) -> Result<Welcome, String> {
    let task = transfer::start_fetch(peer.world().resource::<Iroh>(), link.clone());
    wait(&mut [host, peer], task)
}

fn comment(author: &App, text: &str) -> CommentOp {
    CommentOp::Add(Comment {
        id: uuid::Uuid::new_v4(),
        parent: None,
        author: id_of(author),
        author_name: "Viewer".into(),
        time: 0,
        text: text.into(),
        tag: None,
        assignee: None,
        resolved: false,
        edited: None,
    })
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cadrs-host-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A host whose data dir holds `doc`, shared with `members` (all of them its users); and the
/// link to it.
fn host_with(dir: &Path, doc: &Document, members: Vec<Member>) -> (App, ShareLink) {
    Users::edit(dir, |u| {
        for m in &members {
            if u.by_id(&m.id).is_none() {
                u.users.push(User {
                    name: m.name.clone(),
                    role: Role::Collaborator,
                    sign_in: None,
                    ids: vec![m.id.clone()],
                });
            }
        }
        Ok(())
    })
    .unwrap();
    let mut meta = DocumentMeta::new("host", 0);
    let mut sharing = Sharing::new(TOPIC);
    sharing.members = members;
    meta.sharing = Some(sharing);
    Store::new(dir).create(doc, &meta).unwrap();
    let mut host = App::new();
    host.add_plugins(MinimalPlugins);
    cadrs_host::add(&mut host, dir, "Team host", None, Relays::Disabled);
    let link = ShareLink {
        document: doc.id,
        name: doc.name.clone(),
        topic: TOPIC,
        owner: host.world().resource::<Iroh>().addr().unwrap(),
    };
    (host, link)
}

#[test]
fn the_host_sends_its_document_numbers_edits_and_saves_them() {
    let mut editor = peer("Editor");
    let mut viewer = peer("Viewer");
    let mut stranger = peer("Stranger");

    // A shared document the host owns, in its data dir before it starts.
    let dir = temp_dir();
    let store = Store::new(&dir);
    let doc = Document::new("Frame");
    let members = vec![
        member(&editor, "Editor", Permission::Edit),
        member(&viewer, "Viewer", Permission::View),
    ];
    let (mut host, link) = host_with(&dir, &doc, members);

    // The editor gets the document, as the host has it.
    let welcome = fetch(&mut host, &mut editor, &link).unwrap();
    assert_eq!(welcome.access.permission, Permission::Edit);
    assert_eq!(welcome.owner_name, "Team host");
    assert_eq!(welcome.snapshot.seq, 0);
    assert_eq!(welcome.snapshot.document, doc);

    // Their edit is numbered 1.
    let mut renamed = doc.clone();
    renamed.name = "Frame v2".into();
    let change = Change::between(&doc, &renamed);
    let iroh = editor.world().resource::<Iroh>();
    let task = transfer::start_propose(iroh, &link, welcome.epoch, 1, 0, &change);
    assert_eq!(
        wait(&mut [&mut host, &mut editor], task),
        Ok(Verdict::Accepted(1))
    );

    // An edit from another session of the host's is sent back for a fresh copy.
    let iroh = editor.world().resource::<Iroh>();
    let task = transfer::start_propose(iroh, &link, welcome.epoch ^ 2, 2, 1, &change);
    assert_eq!(
        wait(&mut [&mut host, &mut editor], task),
        Ok(Verdict::Resync)
    );

    // The editor makes a version of it as it is now.
    let iroh = editor.world().resource::<Iroh>();
    let task = transfer::start_create_version(
        iroh,
        &link,
        welcome.epoch,
        "Before review".into(),
        String::new(),
    );
    assert_eq!(
        wait(&mut [&mut host, &mut editor], task),
        Ok(Verdict::Accepted(0))
    );

    // The viewer gets the document with the edit in it, and may not edit.
    let seen = fetch(&mut host, &mut viewer, &link).unwrap();
    assert_eq!(seen.access.permission, Permission::View);
    assert_eq!(seen.snapshot.seq, 1);
    assert_eq!(seen.snapshot.document.name, "Frame v2");
    let iroh = viewer.world().resource::<Iroh>();
    let task = transfer::start_propose(iroh, &link, seen.epoch, 1, 1, &change);
    assert!(matches!(
        wait(&mut [&mut host, &mut viewer], task),
        Ok(Verdict::Refused(_))
    ));

    // Nor make a version; but they may comment, in their own name only.
    let iroh = viewer.world().resource::<Iroh>();
    let task = transfer::start_create_version(iroh, &link, seen.epoch, "Mine".into(), "".into());
    assert!(matches!(
        wait(&mut [&mut host, &mut viewer], task),
        Ok(Verdict::Refused(_))
    ));
    let iroh = viewer.world().resource::<Iroh>();
    let task = transfer::start_comment(iroh, &link, comment(&viewer, "Thicker arms?"));
    assert_eq!(
        wait(&mut [&mut host, &mut viewer], task),
        Ok(Verdict::Accepted(0))
    );
    let iroh = viewer.world().resource::<Iroh>();
    let task = transfer::start_comment(iroh, &link, comment(&editor, "Not mine"));
    assert!(matches!(
        wait(&mut [&mut host, &mut viewer], task),
        Ok(Verdict::Refused(_))
    ));

    // Someone the host doesn't know gets nothing, nor does a wrong room.
    assert!(fetch(&mut host, &mut stranger, &link).is_err());
    let wrong = ShareLink {
        topic: [0; 32],
        ..link.clone()
    };
    assert!(fetch(&mut host, &mut editor, &wrong).is_err());

    // The edit reaches the disk a moment later.
    let start = Instant::now();
    loop {
        host.update();
        if store.load(doc.id).unwrap().document.name == "Frame v2" {
            break;
        }
        assert!(start.elapsed() < PATIENCE, "the host never saved the edit");
        std::thread::sleep(Duration::from_millis(16));
    }
    // So do its history (the edit, as the editor's, and the version) and the comment.
    let mut log = HistoryLog::load(&store, doc.id).unwrap().unwrap();
    assert_eq!(log.head().name, "Frame v2");
    assert_eq!(log.len(), 2);
    let versions: Vec<&str> = log.versions().iter().map(|v| v.name()).collect();
    assert_eq!(versions, ["Before review"]);
    let comments = Comments::load(&store.doc_dir(doc.id));
    assert_eq!(comments.0.len(), 1);
    assert_eq!(comments.0[0].text, "Thicker arms?");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_host_keeps_the_tabs_someone_has_open_for_themselves() {
    let mut ann = peer("Ann");
    let mut ben = peer("Ben");
    let dir = temp_dir();
    let doc = Document::new("Frame");
    let members = vec![
        member(&ann, "Ann", Permission::Edit),
        member(&ben, "Ben", Permission::Edit),
    ];
    let (mut host, link) = host_with(&dir, &doc, members);
    let ann_epoch = fetch(&mut host, &mut ann, &link).unwrap().epoch;
    let ben_epoch = fetch(&mut host, &mut ben, &link).unwrap().epoch;
    let tab = doc.elements[1].id;
    let renamed = |name: &str| {
        let mut d = doc.clone();
        d.elements[1].name = name.into();
        Change::between(&doc, &d)
    };

    // Ann opens the tab for herself: Ben's edit to it is undone, and he can't just take it.
    let task = transfer::start_lock(ann.world().resource::<Iroh>(), &link, tab, true, false);
    assert_eq!(
        wait(&mut [&mut host, &mut ann], task),
        Ok(Verdict::Accepted(0))
    );
    let iroh = ben.world().resource::<Iroh>();
    let task = transfer::start_propose(iroh, &link, ben_epoch, 1, 0, &renamed("Ben's"));
    assert!(matches!(
        wait(&mut [&mut host, &mut ben], task),
        Ok(Verdict::Refused(_))
    ));
    let task = transfer::start_lock(ben.world().resource::<Iroh>(), &link, tab, true, false);
    assert!(matches!(
        wait(&mut [&mut host, &mut ben], task),
        Ok(Verdict::Refused(_))
    ));
    // Nor give it back for her (his Unlock changes nothing).
    let task = transfer::start_lock(ben.world().resource::<Iroh>(), &link, tab, false, false);
    wait(&mut [&mut host, &mut ben], task).unwrap();
    let iroh = ben.world().resource::<Iroh>();
    let task = transfer::start_propose(iroh, &link, ben_epoch, 2, 0, &renamed("Ben's"));
    assert!(matches!(
        wait(&mut [&mut host, &mut ben], task),
        Ok(Verdict::Refused(_))
    ));

    // Ann's own edit to it goes through.
    let iroh = ann.world().resource::<Iroh>();
    let task = transfer::start_propose(iroh, &link, ann_epoch, 1, 0, &renamed("Ann's"));
    assert_eq!(
        wait(&mut [&mut host, &mut ann], task),
        Ok(Verdict::Accepted(1))
    );

    // Ben takes it over: now Ann's edits to it are undone.
    let task = transfer::start_lock(ben.world().resource::<Iroh>(), &link, tab, true, true);
    assert_eq!(
        wait(&mut [&mut host, &mut ben], task),
        Ok(Verdict::Accepted(0))
    );
    let iroh = ann.world().resource::<Iroh>();
    let task = transfer::start_propose(iroh, &link, ann_epoch, 2, 1, &renamed("Ann's again"));
    assert!(matches!(
        wait(&mut [&mut host, &mut ann], task),
        Ok(Verdict::Refused(_))
    ));

    // He gives it back, and Ann may edit it again.
    let task = transfer::start_lock(ben.world().resource::<Iroh>(), &link, tab, false, false);
    assert_eq!(
        wait(&mut [&mut host, &mut ben], task),
        Ok(Verdict::Accepted(0))
    );
    let iroh = ann.world().resource::<Iroh>();
    let task = transfer::start_propose(iroh, &link, ann_epoch, 3, 1, &renamed("Ann's again"));
    assert_eq!(
        wait(&mut [&mut host, &mut ann], task),
        Ok(Verdict::Accepted(2))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn whoever_may_share_adds_people_who_get_the_link_and_may_leave() {
    let mut ann = peer("Ann");
    let mut vic = peer("Vic");
    let mut ned = peer("Ned");
    let mut mia = peer("Mia");
    let dir = temp_dir();
    let store = Store::new(&dir);
    let doc = Document::new("Frame");
    // Ned is a user the document isn't shared with yet; Mia maintains it, unshared too.
    user(&dir, &ned, "Ned", Role::Collaborator);
    user(&dir, &mia, "Mia", Role::Maintainer);
    Users::edit(&dir, |u| u.set_maintainer(doc.id, "Mia", true)).unwrap();
    let members = vec![
        member(&ann, "Ann", Permission::Edit),
        member(&vic, "Vic", Permission::View),
    ];
    let (mut host, link) = host_with(&dir, &doc, members);
    let edit = Access {
        permission: Permission::Edit,
        rights: Rights::of(Permission::Edit),
    };

    // Ned tries the link first, and is refused. (The test has no relays or address lookup, so
    // this is also how the host learns where Ned is, to send the link later.)
    assert!(fetch(&mut host, &mut ned, &link).is_err());

    // Vic, who may only view, may not share; Ann may.
    let iroh = vic.world().resource::<Iroh>();
    let task = transfer::start_add_member(iroh, &link, id_of(&ned), "Ned".into(), edit);
    assert!(matches!(
        wait(&mut [&mut host, &mut vic], task),
        Ok(Verdict::Refused(_))
    ));
    let iroh = ann.world().resource::<Iroh>();
    let task = transfer::start_add_member(iroh, &link, id_of(&ned), "Ned".into(), edit);
    assert_eq!(
        wait(&mut [&mut host, &mut ann], task),
        Ok(Verdict::Accepted(0))
    );
    // Nobody adds someone who isn't a user; Mia, who maintains it, shares it as she likes.
    let quinn = cadrs_net::bevy_iroh::iroh::SecretKey::from_bytes(&[9; 32]).public();
    let iroh = mia.world().resource::<Iroh>();
    let task = transfer::start_add_member(iroh, &link, quinn.to_string(), "Quinn".into(), edit);
    assert!(matches!(
        wait(&mut [&mut host, &mut mia], task),
        Ok(Verdict::Refused(_))
    ));
    let iroh = mia.world().resource::<Iroh>();
    let task = transfer::start_add_member(iroh, &link, id_of(&vic), "Vic".into(), edit);
    assert_eq!(
        wait(&mut [&mut host, &mut mia], task),
        Ok(Verdict::Accepted(0))
    );

    // The host sends Ned the link, which Ned's cadrs takes.
    let start = Instant::now();
    let invite = loop {
        host.update();
        ned.update();
        let asks = ned.world_mut().resource_mut::<Requests>().drain();
        if let Some(r) = asks.into_iter().find(|r| matches!(r.ask, Ask::Invite(_))) {
            let Ask::Invite(invite) = &r.ask else {
                unreachable!()
            };
            let invite = invite.clone();
            r.verdict(Verdict::Accepted(0));
            break invite;
        }
        assert!(start.elapsed() < PATIENCE, "the host never sent the link");
        std::thread::sleep(Duration::from_millis(16));
    };
    assert_eq!(invite.access.permission, Permission::Edit);
    assert_eq!(invite.owner_name, "Team host");
    let sent: ShareLink = invite.link.parse().unwrap();
    assert_eq!((sent.document, sent.topic), (doc.id, TOPIC));
    assert_eq!(sent.owner.id, link.owner.id);

    // Ned can open the document, and the host notes that Ned has the link.
    let welcome = fetch(&mut host, &mut ned, &link).unwrap();
    assert_eq!(welcome.access.permission, Permission::Edit);
    let start = Instant::now();
    loop {
        host.update();
        let meta = store.load(doc.id).unwrap().meta;
        let sharing = meta.sharing.unwrap();
        let noted = sharing.members.iter().find(|m| m.id == id_of(&ned));
        if noted.is_some_and(|m| m.delivered == Some(TOPIC)) {
            break;
        }
        assert!(start.elapsed() < PATIENCE, "the host never noted the link");
        std::thread::sleep(Duration::from_millis(16));
    }

    // Ned leaves, and can no longer open it.
    let task = transfer::start_leave(ned.world().resource::<Iroh>(), &link);
    assert_eq!(
        wait(&mut [&mut host, &mut ned], task),
        Ok(Verdict::Accepted(0))
    );
    assert!(fetch(&mut host, &mut ned, &link).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_host_takes_back_what_an_editor_kept_while_it_was_away() {
    let mut ann = peer("Ann");
    let mut vic = peer("Vic");
    let dir = temp_dir();
    let doc = Document::new("Frame");
    let members = vec![
        member(&ann, "Ann", Permission::Edit),
        member(&vic, "Vic", Permission::View),
    ];
    let (mut host, link) = host_with(&dir, &doc, members);

    // What Ann kept in order while the host was down: the frame renamed.
    let mut kept = doc.clone();
    kept.name = "Frame (edited offline)".into();
    let snapshot = Snapshot {
        seq: 0,
        document: kept,
        blobs: Vec::new(),
    };

    // Someone who may only view can't hand it over; Ann can, and it is the host's again.
    let task = transfer::start_handover(vic.world().resource::<Iroh>(), &link, &snapshot);
    assert!(matches!(
        wait(&mut [&mut host, &mut vic], task),
        Ok(Verdict::Refused(_))
    ));
    let task = transfer::start_handover(ann.world().resource::<Iroh>(), &link, &snapshot);
    assert_eq!(
        wait(&mut [&mut host, &mut ann], task),
        Ok(Verdict::Accepted(0))
    );
    let seen = fetch(&mut host, &mut vic, &link).unwrap();
    assert_eq!(seen.snapshot.document.name, "Frame (edited offline)");
    assert_eq!(seen.snapshot.seq, 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_maintainer_gives_the_host_a_document_and_everyone_follows_it() {
    let mut olga = peer("Olga");
    let mut pat = peer("Pat");
    let mut sam = peer("Sam");
    let dir = temp_dir();
    user(&dir, &olga, "Olga", Role::Maintainer);
    user(&dir, &pat, "Pat", Role::Collaborator);
    user(&dir, &sam, "Sam", Role::Collaborator);
    let mut host = App::new();
    host.add_plugins(MinimalPlugins);
    cadrs_host::add(&mut host, &dir, "Team host", None, Relays::Disabled);
    let host_addr = host.world().resource::<Iroh>().addr().unwrap();

    // Olga's document, shared with Pat, which she gives to the host.
    let doc = Document::new("Bracket");
    let mut sharing = Sharing::new(TOPIC);
    sharing.members = vec![member(&pat, "Pat", Permission::Edit)];
    sharing.members[0].delivered = Some(TOPIC);
    let ownership = Ownership {
        snapshot: Snapshot {
            seq: 0,
            document: doc.clone(),
            blobs: Vec::new(),
        },
        sharing,
        comments: Comments::default(),
        from_name: "Olga".into(),
    };
    let link = ShareLink {
        document: doc.id,
        name: doc.name.clone(),
        topic: TOPIC,
        owner: host_addr.clone(),
    };
    // (The test has no address lookup: each reaches the host once, so they can dial it by ID.)
    assert!(fetch(&mut host, &mut olga, &link).is_err());
    assert!(fetch(&mut host, &mut sam, &link).is_err());

    // Sam, a collaborator, can't; Olga, a maintainer, can.
    let iroh = sam.world().resource::<Iroh>();
    let task = transfer::start_transfer(iroh, host_addr.id, &link, &ownership);
    assert!(matches!(
        wait(&mut [&mut host, &mut sam], task),
        Ok(Verdict::Refused(_))
    ));
    let iroh = olga.world().resource::<Iroh>();
    let task = transfer::start_transfer(iroh, host_addr.id, &link, &ownership);
    assert_eq!(
        wait(&mut [&mut host, &mut olga], task),
        Ok(Verdict::Accepted(0))
    );

    // The host owns it now: Pat and Olga, who may both edit, open it from the host.
    let welcome = fetch(&mut host, &mut pat, &link).unwrap();
    assert_eq!(welcome.owner_name, "Team host");
    assert_eq!(welcome.access.permission, Permission::Edit);
    assert_eq!(welcome.snapshot.document.name, "Bracket");
    let welcome = fetch(&mut host, &mut olga, &link).unwrap();
    assert_eq!(welcome.access.permission, Permission::Edit);
    assert!(fetch(&mut host, &mut sam, &link).is_err());
    // And Olga maintains it.
    let users = Users::read(&dir).unwrap();
    assert_eq!(users.maintainers_of(doc.id), ["Olga"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_host_stays_out_of_the_public_room_unless_turned_on() {
    let dir = temp_dir();
    let in_public_room = |dir: &Path| {
        let mut host = App::new();
        host.add_plugins(MinimalPlugins);
        cadrs_host::add(&mut host, dir, "Team host", None, Relays::Disabled);
        host.world()
            .resource::<cadrs_net::public::PublicEnabled>()
            .0
    };
    assert!(!in_public_room(&dir));
    cadrs_host::set_public(&dir, true).unwrap();
    assert!(in_public_room(&dir));
    cadrs_host::set_public(&dir, false).unwrap();
    assert!(!in_public_room(&dir));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_host_stopped_and_started_again_has_everything() {
    let mut ann = peer("Ann");
    let dir = temp_dir();
    let doc = Document::new("Frame");
    let members = vec![member(&ann, "Ann", Permission::Edit)];
    let (mut host, link) = host_with(&dir, &doc, members);
    let welcome = fetch(&mut host, &mut ann, &link).unwrap();

    // An edit, then the host is stopped at once: it still saves the edit on the way out.
    let mut d = doc.clone();
    d.name = "Frame v2".into();
    let iroh = ann.world().resource::<Iroh>();
    let task =
        transfer::start_propose(iroh, &link, welcome.epoch, 1, 0, &Change::between(&doc, &d));
    assert_eq!(
        wait(&mut [&mut host, &mut ann], task),
        Ok(Verdict::Accepted(1))
    );
    host.world_mut().write_message(AppExit::Success);
    host.update();
    drop(host);

    // Started again on the same data dir, it has the document, its history and its sharing.
    let mut host = App::new();
    host.add_plugins(MinimalPlugins);
    cadrs_host::add(&mut host, &dir, "Team host", None, Relays::Disabled);
    let link = ShareLink {
        owner: host.world().resource::<Iroh>().addr().unwrap(),
        ..link
    };
    let again = fetch(&mut host, &mut ann, &link).unwrap();
    assert_eq!(again.snapshot.document.name, "Frame v2");
    assert_ne!(again.epoch, welcome.epoch);
    let mut log = HistoryLog::load(&Store::new(&dir), doc.id)
        .unwrap()
        .unwrap();
    assert_eq!(log.head().name, "Frame v2");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_host_answers_only_its_users_and_takes_new_ones_as_they_come() {
    let mut ann = peer("Ann");
    let mut zoe = peer("Zoe");
    let dir = temp_dir();
    let doc = Document::new("Frame");
    let (mut host, link) = host_with(&dir, &doc, vec![member(&ann, "Ann", Permission::Edit)]);
    // Link sharing on: anyone with the link may edit, if the host knows them.
    let mut meta = Store::new(&dir).load(doc.id).unwrap().meta;
    meta.sharing.as_mut().unwrap().link = Some(Permission::Edit);
    Store::new(&dir).save(&doc, &meta).unwrap();
    let welcome = fetch(&mut host, &mut ann, &link).unwrap();

    // Zoe has the link, but isn't a user: she gets nothing, and may do nothing.
    let refused = fetch(&mut host, &mut zoe, &link).unwrap_err();
    assert!(refused.contains("doesn't know"), "{refused}");
    let mut d = doc.clone();
    d.name = "Zoe's".into();
    let iroh = zoe.world().resource::<Iroh>();
    let task =
        transfer::start_propose(iroh, &link, welcome.epoch, 1, 0, &Change::between(&doc, &d));
    assert!(matches!(
        wait(&mut [&mut host, &mut zoe], task),
        Ok(Verdict::Refused(_))
    ));

    // Made a user while the host runs, she opens it within seconds, with the link's access.
    user(&dir, &zoe, "Zoe", Role::Collaborator);
    let start = Instant::now();
    let welcome = loop {
        if let Ok(w) = fetch(&mut host, &mut zoe, &link) {
            break w;
        }
        assert!(
            start.elapsed() < PATIENCE,
            "the host never took the new user"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(welcome.access.permission, Permission::Edit);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `cadrs <args> --data-dir <dir>`, for one of the commands: its exit code.
fn command(dir: &Path, args: &[&str]) -> u8 {
    let mut args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    args.extend(["--data-dir".to_string(), dir.display().to_string()]);
    match cadrs_host::admin::parse(&args) {
        Some(Ok(a)) => cadrs_host::admin::run(a),
        Some(Err(e)) => panic!("{e}"),
        None => panic!("not a command: {args:?}"),
    }
}

#[test]
fn the_commands_look_after_users_and_maintainers() {
    let dir = temp_dir();
    let doc = Document::new("Bracket");
    Store::new(&dir)
        .create(&doc, &DocumentMeta::new("host", 0))
        .unwrap();
    let key = |n: u8| {
        cadrs_net::bevy_iroh::iroh::SecretKey::from_bytes(&[n; 32])
            .public()
            .to_string()
    };
    let (laptop, browser) = (key(1), key(2));

    // A maintainer with two cadrs IDs, and a collaborator.
    let id = laptop.as_str();
    let args = ["--host-user", "Max", "--role", "maintainer", "--id", id];
    assert_eq!(command(&dir, &args), 0);
    assert_eq!(command(&dir, &["--host-user", "max", "--id", &browser]), 0);
    assert_eq!(command(&dir, &["--host-user", "Cleo"]), 0);
    let users = Users::read(&dir).unwrap();
    assert_eq!(users.role(&browser), Some(Role::Maintainer));
    assert_eq!(users.by_name("Cleo").unwrap().role, Role::Collaborator);
    // An ID is one person's; a name is a cadrs ID's only.
    assert_eq!(command(&dir, &["--host-user", "Cleo", "--id", &laptop]), 1);
    assert_eq!(command(&dir, &["--host-user", "Cleo", "--id", "nope"]), 1);

    // Max maintains the document, by name; not Cleo, a collaborator.
    assert_eq!(
        command(&dir, &["--host-document", "bracket", "--maintainer", "Max"]),
        0
    );
    assert_eq!(
        command(
            &dir,
            &["--host-document", "Bracket", "--maintainer", "Cleo"]
        ),
        1
    );
    assert_eq!(command(&dir, &["--host-document", "Nothing"]), 1);
    assert!(Users::read(&dir).unwrap().maintains(&laptop, doc.id));
    assert_eq!(command(&dir, &["--host-documents"]), 0);
    assert_eq!(command(&dir, &["--host-users"]), 0);

    // Renamed, still the maintainer; made a collaborator, no longer; then removed.
    assert_eq!(
        command(&dir, &["--host-user", "Max", "--rename", "Maxine"]),
        0
    );
    let users = Users::read(&dir).unwrap();
    assert_eq!(users.maintainers_of(doc.id), ["Maxine"]);
    let args = [
        "--host-user",
        "Maxine",
        "--role",
        "collaborator",
        "--forget-id",
        id,
    ];
    assert_eq!(command(&dir, &args), 0);
    let users = Users::read(&dir).unwrap();
    assert!(users.maintainers.is_empty());
    assert_eq!(
        users.by_name("Maxine").unwrap().ids,
        std::slice::from_ref(&browser)
    );
    assert_eq!(command(&dir, &["--host-user", "Maxine", "--remove"]), 0);
    assert!(Users::read(&dir).unwrap().by_id(&browser).is_none());

    // Options that don't go with the command, and no command.
    let bad = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        matches!(cadrs_host::admin::parse(&args), Some(Err(_)))
    };
    assert!(bad(&["--host-users", "--role", "admin"]));
    assert!(bad(&["--host-user", "Ada", "--role", "boss"]));
    assert!(cadrs_host::admin::parse(&["--host".to_string()]).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}
