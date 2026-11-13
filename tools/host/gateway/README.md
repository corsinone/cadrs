# Sign-in gateway for the host's admin page

The host serves its admin page only on the machine itself (`127.0.0.1`). It believes whoever
the gateway names in the `Remote-User` header. The gateway is two containers:

- [Caddy](https://caddyserver.com) serves HTTPS and lets a request through only once Authelia
  says someone is signed in.
- [Authelia](https://www.authelia.com) shows the sign-in page and checks passwords against the
  team's LDAP.

The page then shows that person what their role on the host allows: admins see everything,
maintainers see the documents they maintain, and anyone else is turned away. The host itself
never sees a password.

## Before you start

- **Two names in the lab's DNS** for the team server machine, one for Authelia's sign-in page
  and one for the admin page. Both go under a shared domain: `auth.lab.example.org` and
  `cadrs.lab.example.org` below.
- **An LDAP account for searching** the directory: its DN and password. Ask whoever runs the
  LDAP, and find out whether it is OpenLDAP or Active Directory, its address, and where the
  people and groups are (the base DN).
- Docker with Compose on the team server machine, where the host already runs
  (`../README.md`).

## Set it up

1. Copy this folder to the team server machine, say to `/srv/cadrs-host/gateway`.
2. Replace `lab.example.org` with the lab's domain in `Caddyfile` and in
   `authelia/configuration.yml`.
3. Fill in the `ldap` section of `authelia/configuration.yml`: address, `implementation`,
   `base_dn`, where the users and groups are, the search account (`user`) and the username
   attribute (`uid` for OpenLDAP, `sAMAccountName` for Active Directory).
4. Make the secrets, which only the containers read:

   ```sh
   mkdir -m 700 secrets
   for s in jwt session storage; do openssl rand -hex 64 > secrets/$s; done
   printf '%s' '<the search account password>' > secrets/ldap
   chmod 600 secrets/*
   ```

5. Turn the admin page on, and give each admin and maintainer their LDAP username as their
   sign-in:

   ```sh
   docker exec cadrs-host cadrs --host-settings --page 127.0.0.1:8090 --data-dir /data
   docker exec cadrs-host cadrs --host-user "Ada" --sign-in ada --data-dir /data
   docker restart cadrs-host
   ```

   The page starts with the host. Its log says `Admin page on http://127.0.0.1:8090...`.

6. Start the gateway: `docker compose up -d`, then `docker compose logs authelia` should show
   that it reached the LDAP.

7. Open `https://cadrs.lab.example.org`. You are sent to the sign-in page, then back to the
   admin page, signed in as yourself.

## Certificates

`tls internal` in the `Caddyfile` makes certificates with Caddy's own authority. Browsers warn
about them until that authority is trusted. You can install it on the team's machines (it is in
the `caddy-data` volume, at `caddy/pki/authorities/local/root.crt`). Or, with certificates from
the lab, write `tls /certs/cert.pem /certs/key.pem` in the `Caddyfile` instead and mount them
into the Caddy container.

## Notes

- Authelia lets in anyone in the LDAP (`one_factor`); the host's users and roles decide the
  rest. To let only an LDAP group through, add `subject: 'group:<name>'` to the rule in
  `authelia/configuration.yml`.
- Caddy drops `Remote-User` and the other names sent by a browser, so only Authelia names
  anyone. Keep the admin page on `127.0.0.1`, which the host insists on, so nothing gets
  around Caddy.
- Every change made on the page goes into the host's log with who made it
  (`docker logs cadrs-host`).
- The image versions are pinned in `compose.yaml` (`authelia/authelia:4.39`, `caddy:2.10`).
  Before moving to newer ones, read Authelia's release notes, as its configuration sometimes
  changes.
