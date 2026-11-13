# The team host

`cadrs --host` in a container: the always-online owner of a team's shared documents
(`crates/cadrs_host`). It keeps their rooms, sends each document to whoever it is shared with
and numbers everyone's edits, so the team works on them while every laptop is off. Its identity
(and so its cadrs ID), its settings (`host.ron`), its users (`users.ron`) and the documents it
owns are in `/data`.

The image is Ubuntu 26.04, as the build machine, for its glibc. To build it and start the host
on a server, with the data in `/srv/cadrs-host`:

```sh
cargo build -r && strip -o /tmp/cadrs target/release/cadrs
scp /tmp/cadrs tools/host/Dockerfile <server>:/srv/cadrs-host/build/
ssh <server> 'cd /srv/cadrs-host/build && docker build -t cadrs-host . \
  && docker rm -f cadrs-host && docker run -d --name cadrs-host --restart unless-stopped \
  --network host -v /srv/cadrs-host/data:/data cadrs-host --name "Team host"'
```

`docker logs cadrs-host` shows its cadrs ID. It stays out of the public room unless an admin
turns it on (`--host-settings --public on` below, kept in `/data/host.ron`).

The host answers only its users (`/data/users.ron`), each a person with a role and the cadrs
IDs they use, one for each device: maintainers and admins give it documents with **Transfer
ownership** and maintain them; collaborators work on what is shared with them. The commands
that look after them run in the container, while the host runs (it takes a change within
seconds):

```sh
docker exec cadrs-host cadrs --host-user "Ada" --role admin --id <cadrs ID> --data-dir /data
docker exec cadrs-host cadrs --host-users --data-dir /data
docker exec cadrs-host cadrs --host-documents --data-dir /data
docker exec cadrs-host cadrs --host-document "Bracket" --maintainer "Ada" --data-dir /data
docker exec cadrs-host cadrs --host-document "Bracket" --edit "Ada" --view "Bob" --data-dir /data
docker exec cadrs-host cadrs --host-settings --public on --data-dir /data
```

Sharing a document with a user shares it with each of their cadrs IDs; the host makes the
change as it would one from the app's Share dialog, so everyone in the document hears it, and
`--unshare` (without link sharing) or `--link off` moves the document to a fresh room whose link
only its members get. `cadrs --help` lists every option (`--rename`, `--remove`,
`--forget-id`...).

`docker stop cadrs-host` sends Ctrl+C (the image's `STOPSIGNAL`): the host saves every edit and
history log before it stops.

To back it up while it runs, copy its data into a new folder, which the command reads back
(an external tool, restic or borg, then keeps the copies):

```sh
docker exec cadrs-host cadrs --host-backup /data-backups/$(date +%F) --data-dir /data
```

with a second volume for the copies (`-v /srv/cadrs-host/backups:/data-backups` on `docker run`).
A copy holds the host's key (`identity.key`): keep it as private as the host. To restore one,
stop the host and put the copy in place of `/data`; `cadrs --document <name> --check --data-dir
<copy>` tries a document of it first.
