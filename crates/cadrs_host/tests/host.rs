//! A host and its peers in one process, over real iroh with no relay: the host owns a shared
//! document in its data dir, an editor fetches it and edits it, a viewer and a stranger are
//! kept to what they may do, and the edit is on disk afterwards.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use bevy::prelude::*;
use cadrs_core::collab::sync::Change;
use cadrs_core::collab::{Member, Permission, Sharing};
use cadrs_core::library::DocumentMeta;
use cadrs_core::{Document, Store};
use cadrs_net::bevy_iroh::{Iroh, IrohTask, Relays};
use cadrs_net::{NetPlugin, ShareLink, Verdict, Welcome, transfer};

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

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cadrs-host-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
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
    let mut meta = DocumentMeta::new("host", 0);
    let mut sharing = Sharing::new(TOPIC);
    sharing.members = vec![
        member(&editor, "Editor", Permission::Edit),
        member(&viewer, "Viewer", Permission::View),
    ];
    meta.sharing = Some(sharing);
    store.create(&doc, &meta).unwrap();

    let mut host = App::new();
    host.add_plugins(MinimalPlugins);
    cadrs_host::add(&mut host, &dir, "Team host", None, Relays::Disabled);
    let link = ShareLink {
        document: doc.id,
        name: doc.name.clone(),
        topic: TOPIC,
        owner: host.world().resource::<Iroh>().addr().unwrap(),
    };

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

    // Someone the document is not shared with gets nothing, nor does a wrong room.
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
    let _ = std::fs::remove_dir_all(&dir);
}
