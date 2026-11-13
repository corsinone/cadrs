//! The admin page, over HTTP as the sign-in gateway would reach it: who may see it, what each
//! role sees and changes, forms from elsewhere refused, and its address kept to the machine.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use cadrs_core::collab::Sharing;
use cadrs_core::library::DocumentMeta;
use cadrs_core::{Document, Store};
use cadrs_host::users::{Role, User, Users};

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cadrs-page-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn key(n: u8) -> String {
    cadrs_net::bevy_iroh::iroh::SecretKey::from_bytes(&[n; 32])
        .public()
        .to_string()
}

fn user(name: &str, role: Role, sign_in: &str, n: u8) -> User {
    User {
        name: name.into(),
        role,
        sign_in: Some(sign_in.into()),
        ids: vec![key(n)],
    }
}

/// One HTTP request, as the gateway sends it: the status and the whole answer.
fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: admin.test\r\nConnection: close\r\n\
         Content-Length: {}\r\nContent-Type: application/x-www-form-urlencoded\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    (out[9..12].parse().unwrap(), out)
}

fn get(addr: SocketAddr, who: &str) -> (u16, String) {
    http(addr, "GET", "/", &[("Remote-User", who)], "")
}

/// A form posted from the page by `who`.
fn post(addr: SocketAddr, who: &str, path: &str, body: &str) -> (u16, String) {
    let headers = [("Remote-User", who), ("Sec-Fetch-Site", "same-origin")];
    http(addr, "POST", path, &headers, body)
}

fn changes(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir.join(cadrs_host::admin::CHANGES_DIR))
        .map(|r| {
            r.filter_map(|e| std::fs::read_to_string(e.ok()?.path()).ok())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn the_admin_page_shows_and_changes_what_each_role_may() {
    let dir = temp_dir();
    let store = Store::new(&dir);
    let frame = Document::new("Frame");
    let bracket = Document::new("Bracket");
    let mut meta = DocumentMeta::new("host", 0);
    meta.sharing = Some(Sharing::new([1; 32]));
    store.create(&frame, &meta).unwrap();
    store
        .create(&bracket, &DocumentMeta::new("host", 0))
        .unwrap();
    Users::edit(&dir, |u| {
        u.users.push(user("Ada", Role::Admin, "ada", 1));
        u.users.push(user("Max", Role::Maintainer, "max", 2));
        u.users.push(user("Cleo", Role::Collaborator, "cleo", 3));
        u.set_maintainer(frame.id, "Max", true)
    })
    .unwrap();
    let any = "127.0.0.1:0".parse().unwrap();
    let addr = cadrs_host::page::start(dir.clone(), any, "Remote-User".into()).unwrap();

    // Only admins and maintainers, as the gateway names them.
    assert_eq!(http(addr, "GET", "/", &[], "").0, 401);
    assert_eq!(get(addr, "zed").0, 403);
    assert_eq!(get(addr, "cleo").0, 403);
    let (status, page) = get(addr, "ADA");
    assert_eq!(status, 200);
    for seen in ["Frame", "Bracket", "<h2>Users", "Cleo", "<h2>Settings"] {
        assert!(page.contains(seen), "an admin sees {seen}");
    }
    // A maintainer sees the documents they maintain, and no users or settings.
    let (status, page) = get(addr, "max");
    assert_eq!(status, 200);
    assert!(page.contains("Frame") && !page.contains("Bracket"));
    assert!(!page.contains("<h2>Users") && !page.contains("<h2>Settings"));

    // A form from elsewhere is refused; one from an origin that is this page's isn't.
    let body = format!("name=Dan&role=collaborator&sign_in=dan&add_id={}", key(4));
    let elsewhere = [("Remote-User", "ada"), ("Sec-Fetch-Site", "cross-site")];
    assert_eq!(http(addr, "POST", "/user", &elsewhere, &body).0, 403);
    assert_eq!(
        http(addr, "POST", "/user", &[("Remote-User", "ada")], &body).0,
        403
    );
    let (status, answer) = post(addr, "ada", "/user", &body);
    assert_eq!(status, 303, "{answer}");
    assert!(answer.contains("said=Added"), "{answer}");
    assert_eq!(
        Users::read(&dir).unwrap().by_sign_in("dan").unwrap().name,
        "Dan"
    );
    let here = [("Remote-User", "ada"), ("Origin", "https://admin.test")];
    assert_eq!(http(addr, "POST", "/settings", &here, "public=on").0, 303);
    let settings = std::fs::read_to_string(dir.join(cadrs_host::SETTINGS_FILE)).unwrap();
    assert!(settings.contains("public: true"), "{settings}");

    // Max shares his document, which goes to the host; not the other one, nor its maintainers.
    let share = "action=share&user=Cleo&permission=view";
    let (status, answer) = post(addr, "max", &format!("/document/{}", frame.id), share);
    assert_eq!(status, 303);
    assert!(answer.contains("said="), "{answer}");
    let asked = changes(&dir);
    assert!(asked.len() == 1 && asked[0].contains("Cleo"), "{asked:?}");
    assert_eq!(
        post(addr, "max", &format!("/document/{}", bracket.id), share).0,
        403
    );
    let maintain = "action=maintainer&user=Dan";
    assert_eq!(
        post(addr, "max", &format!("/document/{}", frame.id), maintain).0,
        403
    );
    // A mistake is said, and nothing goes to the host.
    let (_, answer) = post(
        addr,
        "max",
        &format!("/document/{}", frame.id),
        "action=share&user=Nobody",
    );
    assert!(answer.contains("error="), "{answer}");
    assert_eq!(changes(&dir).len(), 1);
    // Collaborators change nothing.
    assert_eq!(post(addr, "cleo", "/settings", "public=off").0, 403);

    // Ada can't lock herself out.
    let (_, answer) = post(addr, "ada", "/user", "name=Ada&remove=yes");
    assert!(answer.contains("error="), "{answer}");
    assert!(Users::read(&dir).unwrap().by_name("Ada").is_some());

    // What people typed is shown as text.
    let (status, _) = post(
        addr,
        "ada",
        "/user",
        "name=%3Cb%3EEve%3C%2Fb%3E&role=collaborator",
    );
    assert_eq!(status, 303);
    let (_, page) = get(addr, "ada");
    assert!(page.contains("&lt;b&gt;Eve&lt;/b&gt;") && !page.contains("<b>Eve</b>"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_admin_page_listens_only_on_the_machine_itself() {
    let dir = temp_dir();
    let settings = |args: &[&str]| {
        let mut args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        args.extend(["--data-dir".into(), dir.display().to_string()]);
        let admin = cadrs_host::admin::parse(&args).unwrap().unwrap();
        cadrs_host::admin::run(admin)
    };
    assert_eq!(settings(&["--host-settings", "--page", "0.0.0.0:8090"]), 1);
    assert_eq!(
        settings(&["--host-settings", "--page", "127.0.0.1:8090"]),
        0
    );
    let text = std::fs::read_to_string(dir.join(cadrs_host::SETTINGS_FILE)).unwrap();
    assert!(text.contains("127.0.0.1:8090"), "{text}");
    assert_eq!(settings(&["--host-settings", "--page", "off"]), 0);
    let text = std::fs::read_to_string(dir.join(cadrs_host::SETTINGS_FILE)).unwrap();
    assert!(!text.contains("8090"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}
