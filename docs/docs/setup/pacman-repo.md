---
sidebar_position: 3
---

# Pacman Repository

Add the following to your `/etc/pacman.conf` on your target machine to use the repo:

```bash
# nano /etc/pacman.conf
[repo]
SigLevel = Optional TrustAll
Server = http://<server_ip>:8081/$arch
```

## Let the CLI fill it in

`aurcache-cli` is already configured to talk to the instance, and the repository
is the same host on the mirror port — so it can print the stanza with nothing
left to substitute:

```bash
$ aurcache-cli repo config
[repo]
SigLevel = Optional TrustAll
Server = http://192.168.1.10:8081/$arch
```

The stanza goes to stdout and the instructions to stderr, so it appends cleanly:

```bash
aurcache-cli repo config | sudo tee -a /etc/pacman.conf
sudo pacman -Sy
```

This needs no API token — it is arithmetic on the configured URL, not a request
— so it works before the instance is otherwise set up. `--port`, `--name` and
`--siglevel` override the defaults for a deployment that publishes the
repository somewhere else.

`$arch` is pacman's own variable and is deliberately left unexpanded: one stanza
serves every architecture the instance builds for.
