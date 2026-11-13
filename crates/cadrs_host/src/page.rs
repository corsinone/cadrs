//! The admin page: the host's documents, users and settings in a browser. The host serves it
//! on the machine itself (`cadrs --host-settings --page 127.0.0.1:8090`), behind the team's
//! sign-in gateway (`tools/host/gateway`), which signs people in with the team's accounts and
//! names them to the page in a header (`Remote-User`, as Authelia does). The page believes
//! that header, which is why it listens only where the gateway alone reaches it.
//!
//! Whoever signed in is the user with that sign-in. Admins see and change everything: the
//! documents, who maintains them and whom they are shared with, the users, the settings.
//! Maintainers see the documents they maintain and change whom they are shared with.
//! Collaborators, and people who aren't users, get no page. Each change is the command's
//! ([`crate::admin`]): users and maintainers in `users.ron`, sharing left for the host in
//! `changes/`, which it makes within seconds.
//!
//! A change is a form posted from the page itself: the browser says where a form comes from
//! (`Sec-Fetch-Site`, or `Origin`), and one from another site is refused, so that a page
//! elsewhere can't use someone's sign-in to change things.

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path as UrlPath, RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use bevy::log::{error, info};
use cadrs_core::collab::Permission;
use cadrs_core::library::DocumentEntry;

use crate::admin::{self, Share, SharingChange, UserEdit};
use crate::users::{Role, User, Users};

struct Page {
    dir: PathBuf,
    /// Where the gateway names whoever signed in.
    header: String,
}

/// Serves the admin page of the host whose data dir is `dir` on `addr`, on a thread of its own
/// for as long as the process runs: where it listens.
pub fn start(dir: PathBuf, addr: SocketAddr, header: String) -> std::io::Result<SocketAddr> {
    let listener = std::net::TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    let addr = listener.local_addr()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let page = Arc::new(Page { dir, header });
    std::thread::Builder::new()
        .name("cadrs-host-page".into())
        .spawn(move || {
            runtime.block_on(async move {
                let listener = match tokio::net::TcpListener::from_std(listener) {
                    Ok(l) => l,
                    Err(e) => return error!("admin page: {e}"),
                };
                let router = axum::Router::new()
                    .route("/", get(show))
                    .route("/document/{id}", post(document))
                    .route("/user", post(user))
                    .route("/settings", post(settings))
                    .with_state(page);
                if let Err(e) = axum::serve(listener, router).await {
                    error!("admin page: {e}");
                }
            });
        })?;
    Ok(addr)
}

/// A page saying why not.
fn refuse(status: StatusCode, why: &str) -> Response {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>cadrs host</title>{STYLE}\
         <main><h1>cadrs host</h1><p class=error>{}</p></main>",
        esc(why)
    );
    (status, Html(body)).into_response()
}

/// The users, and who signed in, if they may use the page.
fn signed_in(page: &Page, headers: &HeaderMap) -> Result<(Users, User), Box<Response>> {
    let name = headers
        .get(page.header.as_str())
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let Some(name) = name else {
        return Err(Box::new(refuse(
            StatusCode::UNAUTHORIZED,
            "Nobody is signed in: open this page through the team's sign-in page.",
        )));
    };
    let users = Users::read(&page.dir)
        .map_err(|e| Box::new(refuse(StatusCode::INTERNAL_SERVER_ERROR, &e)))?;
    let Some(user) = users.by_sign_in(name).cloned() else {
        return Err(Box::new(refuse(
            StatusCode::FORBIDDEN,
            &format!("{name} isn't one of this host's users: ask one of its admins."),
        )));
    };
    if user.role < Role::Maintainer {
        return Err(Box::new(refuse(
            StatusCode::FORBIDDEN,
            "This page is for the host's admins and maintainers.",
        )));
    }
    Ok((users, user))
}

/// Whether a form comes from this page, as the browser says.
fn from_here(headers: &HeaderMap) -> bool {
    let text = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    match text("sec-fetch-site") {
        Some(site) => site == "same-origin",
        None => match (text("origin"), text("host")) {
            (Some(origin), Some(host)) => origin.split_once("://").map(|(_, h)| h) == Some(host),
            _ => false,
        },
    }
}

/// Who may post the form: signed in, and on this page.
fn poster(page: &Page, headers: &HeaderMap) -> Result<(Users, User), Box<Response>> {
    if !from_here(headers) {
        return Err(Box::new(refuse(
            StatusCode::FORBIDDEN,
            "That change didn't come from this page.",
        )));
    }
    signed_in(page, headers)
}

/// Back to the page, saying how it went.
fn back(done: Result<String, String>) -> Response {
    let (key, text) = match &done {
        Ok(said) => ("said", said.as_str()),
        Err(e) => ("error", e.as_str()),
    };
    let text: String = form_urlencoded::byte_serialize(text.as_bytes()).collect();
    Redirect::to(&format!("/?{key}={text}")).into_response()
}

/// A form's fields.
fn fields(body: &[u8]) -> Vec<(String, String)> {
    form_urlencoded::parse(body).into_owned().collect()
}

fn field<'a>(fields: &'a [(String, String)], name: &str) -> &'a str {
    fields
        .iter()
        .find(|(k, _)| k == name)
        .map_or("", |(_, v)| v.trim())
}

async fn show(
    State(page): State<Arc<Page>>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let (users, me) = match signed_in(&page, &headers) {
        Ok(u) => u,
        Err(r) => return *r,
    };
    let query = fields(query.unwrap_or_default().as_bytes());
    let html = render(
        &page,
        &users,
        &me,
        field(&query, "said"),
        field(&query, "error"),
    );
    ([(header::CACHE_CONTROL, "no-store")], Html(html)).into_response()
}

async fn document(
    State(page): State<Arc<Page>>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (users, me) = match poster(&page, &headers) {
        Ok(u) => u,
        Err(r) => return *r,
    };
    let form = fields(&body);
    let entry = match admin::find(&page.dir, &id) {
        Ok(e) => e,
        Err(e) => return back(Err(e)),
    };
    if !users.manages(&me, entry.id) {
        return refuse(StatusCode::FORBIDDEN, "You don't maintain that document.");
    }
    let action = field(&form, "action");
    if let Some(on) = match action {
        "maintainer" => Some(true),
        "not-maintainer" => Some(false),
        _ => None,
    } {
        if me.role != Role::Admin {
            return refuse(StatusCode::FORBIDDEN, "Only admins choose maintainers.");
        }
        let who = field(&form, "user");
        let done = Users::edit(&page.dir, |u| u.set_maintainer(entry.id, who, on)).map(|_| {
            let what = if on {
                "maintains"
            } else {
                "no longer maintains"
            };
            format!("{who} {what} {}.", entry.name)
        });
        log(&me, &done);
        return back(done);
    }
    let what = match action {
        "share" => Share::With {
            user: field(&form, "user").into(),
            permission: if field(&form, "permission") == "view" {
                Permission::View
            } else {
                Permission::Edit
            },
        },
        "unshare" => Share::Not(field(&form, "user").into()),
        "link" => Share::Link(match field(&form, "link") {
            "view" => Some(Permission::View),
            "edit" => Some(Permission::Edit),
            _ => None,
        }),
        _ => return back(Err("That isn't something the page does.".into())),
    };
    let what = vec![what];
    let done = admin::check_change(&users, &entry, &what).and_then(|_| {
        let change = SharingChange {
            document: entry.id,
            what,
        };
        admin::queue_change(&page.dir, &change).map(|_| {
            format!(
                "Sent to the host, which changes {} within seconds: reload to see it.",
                entry.name
            )
        })
    });
    log(&me, &done);
    back(done)
}

async fn user(State(page): State<Arc<Page>>, headers: HeaderMap, body: Bytes) -> Response {
    let (_, me) = match poster(&page, &headers) {
        Ok(u) => u,
        Err(r) => return *r,
    };
    if me.role != Role::Admin {
        return refuse(StatusCode::FORBIDDEN, "Only admins change users.");
    }
    let form = fields(&body);
    let some = |name: &str| Some(field(&form, name).to_string()).filter(|v| !v.is_empty());
    let edit = UserEdit {
        name: field(&form, "name").into(),
        role: Role::parse(field(&form, "role")),
        sign_in: form
            .iter()
            .any(|(k, _)| k == "sign_in")
            .then(|| field(&form, "sign_in").to_string()),
        ids: some("add_id").into_iter().collect(),
        forget: some("forget_id").into_iter().collect(),
        rename: some("rename"),
        remove: field(&form, "remove") == "yes",
    };
    if edit.name.is_empty() {
        return back(Err("A user needs a name.".into()));
    }
    // Whoever is signed in keeps their own way in; the command line can still change it.
    if edit.name.eq_ignore_ascii_case(&me.name)
        && (edit.remove || edit.role.is_some_and(|r| r != Role::Admin))
    {
        return back(Err(
            "You can't remove yourself or your admin role here: another admin can, or the \
             command line."
                .into(),
        ));
    }
    let done = admin::edit_user(&page.dir, edit).map(|(_, said, name)| match name {
        Some(name) => format!("{said} {name}."),
        None => format!("{said} {}.", field(&form, "name")),
    });
    log(&me, &done);
    back(done)
}

async fn settings(State(page): State<Arc<Page>>, headers: HeaderMap, body: Bytes) -> Response {
    let (_, me) = match poster(&page, &headers) {
        Ok(u) => u,
        Err(r) => return *r,
    };
    if me.role != Role::Admin {
        return refuse(StatusCode::FORBIDDEN, "Only admins change the settings.");
    }
    let on = field(&fields(&body), "public") == "on";
    let done = crate::set_public(&page.dir, on).map(|_| {
        let on = if on { "joins" } else { "leaves" };
        format!("The host {on} the public room within seconds.")
    });
    log(&me, &done);
    back(done)
}

/// Every change made on the page goes in the host's log, with who made it.
fn log(me: &User, done: &Result<String, String>) {
    match done {
        Ok(said) => info!("admin page, {}: {said}", me.name),
        Err(e) => info!("admin page, {}, refused: {e}", me.name),
    }
}

/// Escapes `text` for HTML.
fn esc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

const STYLE: &str = "<style>\
:root{color-scheme:light dark;--line:#8884;--dim:#888;--accent:#2f6fde}\
body{font:15px/1.45 Inter,system-ui,sans-serif;margin:0}\
main{max-width:960px;margin:0 auto;padding:16px}\
h1{font-size:22px;margin:8px 0 4px}h2{font-size:18px;margin:28px 0 8px}\
h3{font-size:16px;margin:0}\
.who,.dim{color:var(--dim)}.id{font:12px ui-monospace,monospace;color:var(--dim);word-break:break-all}\
.said,.error{padding:8px 12px;border-radius:6px;margin:12px 0}\
.said{background:#2f6fde22}.error{background:#d0303022}\
section.card{border:1px solid var(--line);border-radius:8px;padding:12px 14px;margin:10px 0}\
table{border-collapse:collapse;width:100%;margin:6px 0}\
td,th{text-align:left;padding:4px 8px 4px 0;border-bottom:1px solid var(--line);vertical-align:top}\
form{display:inline-flex;gap:6px;flex-wrap:wrap;align-items:center;margin:4px 8px 4px 0}\
input,select,button{font:inherit;padding:3px 6px}\
.line{margin:8px 0}.may{white-space:nowrap}\
.row{display:flex;align-items:center;justify-content:space-between;gap:8px;\
border-bottom:1px solid var(--line)}\
button{cursor:pointer}button.quiet{background:none;border:1px solid var(--line);border-radius:4px}\
</style>";

fn render(page: &Page, users: &Users, me: &User, said: &str, error: &str) -> String {
    let mut h = String::new();
    let _ = write!(
        h,
        "<!doctype html><html lang=en><meta charset=utf-8>\
         <meta name=viewport content='width=device-width,initial-scale=1'>\
         <title>cadrs host</title>{STYLE}<main><h1>cadrs host</h1>\
         <div class=who>Signed in as {} ({})</div>",
        esc(&me.name),
        me.role
    );
    if !said.is_empty() {
        let _ = write!(h, "<div class=said>{}</div>", esc(said));
    }
    if !error.is_empty() {
        let _ = write!(h, "<div class=error>{}</div>", esc(error));
    }
    let admin = me.role == Role::Admin;
    let documents: Vec<DocumentEntry> = admin::owned(&page.dir)
        .into_iter()
        .filter(|e| users.manages(me, e.id))
        .collect();
    let _ = write!(h, "<h2>Documents</h2>");
    if documents.is_empty() {
        let what = if admin {
            "The host has no documents yet: give it one with Transfer ownership."
        } else {
            "You maintain none of the host's documents."
        };
        let _ = write!(h, "<p class=dim>{what}</p>");
    }
    for e in &documents {
        document_card(&mut h, users, admin, e);
    }
    if admin {
        users_section(&mut h, users, &documents);
        let settings = crate::read_settings(&page.dir).unwrap_or_default();
        let (now, other) = if settings.public {
            ("in", "off")
        } else {
            ("out of", "on")
        };
        let _ = write!(
            h,
            "<h2>Settings</h2><div class=line>The host is {now} the public room. \
             <form method=post action='/settings'><input type=hidden name=public value={other}>\
             <button class=quiet>Turn it {other}</button></form></div>"
        );
    }
    h.push_str("</main></html>");
    h
}

/// `<option>`s, the one equal to `chosen` selected.
fn options(h: &mut String, values: &[(&str, &str)], chosen: &str) {
    for (value, label) in values {
        let sel = if *value == chosen { " selected" } else { "" };
        let _ = write!(
            h,
            "<option value='{}'{sel}>{}</option>",
            esc(value),
            esc(label)
        );
    }
}

fn document_card(h: &mut String, users: &Users, admin: bool, e: &DocumentEntry) {
    let action = format!("/document/{}", e.id);
    let trashed = if e.meta.trashed.is_some() {
        " <span class=dim>(in the trash)</span>"
    } else {
        ""
    };
    let _ = write!(
        h,
        "<section class=card><h3>{}{trashed}</h3><div class=id>{}</div>",
        esc(&e.name),
        e.id
    );
    // Maintainers.
    let maintainers = users.maintainers_of(e.id);
    let _ = write!(h, "<div class=line>Maintained by ");
    if maintainers.is_empty() {
        h.push_str("<span class=dim>nobody (the admins manage it)</span>");
    }
    for (i, name) in maintainers.iter().enumerate() {
        if i > 0 {
            h.push_str(", ");
        }
        h.push_str(&esc(name));
        if admin {
            let _ = write!(
                h,
                " <form method=post action='{action}'><input type=hidden name=action \
                 value=not-maintainer><input type=hidden name=user value='{}'>\
                 <button class=quiet title='No longer a maintainer'>×</button></form>",
                esc(name)
            );
        }
    }
    h.push_str("</div>");
    if admin {
        let candidates: Vec<(&str, &str)> = users
            .users
            .iter()
            .filter(|u| u.role >= Role::Maintainer)
            .filter(|u| !maintainers.iter().any(|m| m.eq_ignore_ascii_case(&u.name)))
            .map(|u| (u.name.as_str(), u.name.as_str()))
            .collect();
        if !candidates.is_empty() {
            let _ = write!(
                h,
                "<form method=post action='{action}'><input type=hidden name=action \
                 value=maintainer><select name=user>"
            );
            options(h, &candidates, "");
            h.push_str("</select><button>Add maintainer</button></form>");
        }
    }
    // Whom it is shared with.
    let sharing = e.meta.sharing.as_ref();
    match sharing {
        Some(s) if !s.members.is_empty() => {
            h.push_str("<table><tr><th>Shared with</th><th>May</th><th></th></tr>");
            for m in &s.members {
                let who = match users.by_id(&m.id) {
                    Some(u) if u.name != m.name => {
                        format!("{} <span class=dim>({})</span>", esc(&m.name), esc(&u.name))
                    }
                    Some(_) => esc(&m.name),
                    None => format!(
                        "{} <span class=dim>(not a user, so can't open it)</span>",
                        esc(&m.name)
                    ),
                };
                let _ = write!(
                    h,
                    "<tr><td>{who}<div class=id>{}</div></td><td class=may>{}</td><td>\
                     <form method=post action='{action}'><input type=hidden name=action \
                     value=unshare><input type=hidden name=user value='{}'>\
                     <button class=quiet>Remove</button></form></td></tr>",
                    esc(&m.id),
                    m.permission.label(),
                    esc(&m.id)
                );
            }
            h.push_str("</table>");
        }
        _ => h.push_str("<p class=dim>Not shared with anyone yet.</p>"),
    }
    let names: Vec<(&str, &str)> = users
        .users
        .iter()
        .filter(|u| !u.ids.is_empty())
        .map(|u| (u.name.as_str(), u.name.as_str()))
        .collect();
    if !names.is_empty() {
        let _ = write!(
            h,
            "<form method=post action='{action}'><input type=hidden name=action value=share>\
             Share with <select name=user>"
        );
        options(h, &names, "");
        h.push_str("</select><select name=permission>");
        options(h, &[("edit", "Can edit"), ("view", "Can view")], "edit");
        h.push_str("</select><button>Share</button></form>");
    }
    let link = match sharing.and_then(|s| s.link) {
        Some(Permission::Edit) => "edit",
        Some(Permission::View) => "view",
        None => "off",
    };
    let _ = write!(
        h,
        "<form method=post action='{action}'><input type=hidden name=action value=link>\
         Anyone with the link who is a user: <select name=link>"
    );
    options(
        h,
        &[
            ("off", "No (link sharing off)"),
            ("view", "Can view"),
            ("edit", "Can edit"),
        ],
        link,
    );
    h.push_str("</select><button>Set</button></form></section>");
}

fn users_section(h: &mut String, users: &Users, documents: &[DocumentEntry]) {
    let roles: Vec<(&str, &str)> = Role::ALL.iter().map(|r| (r.label(), r.label())).collect();
    let _ = write!(h, "<h2>Users</h2>");
    for u in &users.users {
        let name = esc(&u.name);
        let maintains: Vec<&str> = documents
            .iter()
            .filter(|e| {
                users
                    .maintainers_of(e.id)
                    .iter()
                    .any(|n| n.eq_ignore_ascii_case(&u.name))
            })
            .map(|e| e.name.as_str())
            .collect();
        let _ = write!(
            h,
            "<section class=card><h3>{name} <span class=dim>{}</span></h3>",
            u.role
        );
        if !maintains.is_empty() {
            let _ = write!(h, "<p>Maintains {}</p>", esc(&maintains.join(", ")));
        }
        let sign_in = u.sign_in.as_deref().unwrap_or("");
        let _ = write!(
            h,
            "<form method=post action='/user'><input type=hidden name=name value='{name}'>\
             <select name=role>"
        );
        options(h, &roles, u.role.label());
        let _ = write!(
            h,
            "</select><input name=sign_in value='{}' placeholder='Sign-in' size=14>\
             <input name=rename placeholder='New name' size=12><button>Change</button></form>",
            esc(sign_in)
        );
        if u.ids.is_empty() {
            h.push_str("<p class=dim>No cadrs ID yet: they can't open documents.</p>");
        }
        for id in &u.ids {
            let _ = write!(
                h,
                "<div class=row><span class=id>{}</span><form method=post action='/user'>\
                 <input type=hidden name=name value='{name}'><input type=hidden name=forget_id \
                 value='{}'><button class=quiet>Forget</button></form></div>",
                esc(id),
                esc(id)
            );
        }
        let _ = write!(
            h,
            "<form method=post action='/user'><input type=hidden name=name \
             value='{name}'><input name=add_id placeholder='cadrs ID of another device' \
             size=40><button>Add ID</button></form><form method=post action='/user'>\
             <input type=hidden name=name value='{name}'><input type=hidden name=remove \
             value=yes><button class=quiet>Remove {name}</button></form></section>"
        );
    }
    let _ = write!(
        h,
        "<section class=card><h3>Add a user</h3><form method=post action='/user'>\
         <input name=name placeholder=Name required size=14><select name=role>"
    );
    options(h, &roles, "collaborator");
    h.push_str(
        "</select><input name=sign_in placeholder='Sign-in' size=14>\
         <input name=add_id placeholder='cadrs ID' size=40><button>Add</button></form></section>",
    );
}
