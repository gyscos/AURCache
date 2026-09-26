---
sidebar_position: 5
---

# Package parsing

Reading a PKGBUILD means *sourcing* it, so every package the server inspects
runs bash in a process that holds the database credentials and owns the
repository. The server never runs `alpm-pkgbuild-bridge` directly: it runs it
through `aurcache-sandbox`, naming both by absolute path, and refuses to parse
at all if that binary is missing. Nothing depends on `PATH`, so nothing about
the environment can quietly unconfine a parse. This holds for every install
method — the server image carries the same sandbox.

Each parse may write only the PKGBUILD's own directory, and cannot read the
server's working directory (the database, the repository and the CA) or
`/etc/aurcache` (where `server.env` keeps the database password and the OAuth
secret). Its environment is replaced, so a PKGBUILD cannot read a secret out of
it either. It also cannot signal the processes around it, which needs Linux
6.12 — the server's minimum.

TCP is denied as well, but that is hardening rather than a boundary: Landlock
only covers TCP, so UDP and DNS still leave the machine. It is there because a
parse has no reason to connect anywhere, not because it contains one that
tries. If a package could be added that you would mind phoning home, a host
firewall is the control that stops it.

## When a package needs the network to parse

About one AUR package in five hundred computes its version while being sourced,
with `pkgver=$(curl -s https://api.github.com/…)` or `git ls-remote`. Denied the
connection, `pkgver` comes out empty and the parse fails on the missing version
rather than on the connection — so the error says which command wanted the
network and names the setting below.

`parse_network` (or `PARSE_NETWORK=true`) lifts the TCP denial for parsing.
Everything else stays: the parse still writes only its own directory, still
cannot read the server's state, still gets no secrets in its environment, and
is still isolated from the processes around it. It is a global setting today,
so turn it on only if you would run those packages' `pkgver` command yourself.
Set it on the Settings page, or in the environment.

`AURCACHE_PROTECTED_DIR` names further directories to keep unreadable
(colon-separated). It adds to the list above rather than replacing it.
