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

`docker logs cadrs-host` shows its cadrs ID. Anything after the image name goes to
`cadrs --host`: `--public on` puts it in the public room, which it stays out of otherwise (kept
in `/data/host.ron`).

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
```

`cadrs --help` lists them all (`--rename`, `--remove`, `--forget-id`...).

`docker stop cadrs-host` sends Ctrl+C (the image's `STOPSIGNAL`): the host saves every edit and
history log before it stops. To back it up, copy `/data` while it is stopped.
