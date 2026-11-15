# Caching

The results of rustc and clippy, and of custom commands with `cache`, are kept in a cache, and fetched from it
rather than run again. The cache is the `cache/dir` property, which is `$XDG_CACHE_HOME/jig`, or on Windows,
`%LOCALAPPDATA%\jig\cache`, or `~/.cache/jig`, by default, and `'off'` turns it off, usually set in the user's
configuration. The `JIG_CACHE` environment variable overrides it, relative to the working directory, while a `JIG_CACHE`
which is empty or `off` turns it off. They are read as the project is configured. A crate which is built again with what
it was built with before, such as once a change to it is undone, or another branch is checked out and back, or once the
build directory is removed, is restored from the cache, with what rustc printed. The cache is found by the contents of
each crate's sources, the crates it uses and the variables it read, and by the identity of the toolchain, from `rustc
-vV` and the files rustup writes when it installs or updates it, so that the project is configured again when the
toolchain is updated in place. Turning the cache on or off configures the project again too. The cache is shared between
checkouts of a project in different directories, as what rustc is given and writes does not depend on where the project
is. A crate or command which is cached runs in a hermetic environment, inheriting only the variables it passes on, with
`pass-env`, and those which locate tools.

`jig cache stats` shows where the cache is and what it holds, `jig cache gc [--max-size <size>]` removes what was used
longest ago until it is no larger than a size, such as `512M` or `10G`, and `jig cache clear` removes everything in it.
After a build, once a day, the cache is collected as by `jig cache gc`, down to the `cache/size` property, `'10G'` by
default, which the `JIG_CACHE_SIZE` environment variable overrides. `jig cache` takes the settings of the user's
configuration, rather than a project's.

```
./build/bootstrap/jig cache stats
JIG_CACHE_SIZE=2G ./build/bootstrap/jig cache gc
JIG_CACHE=off ./build/bootstrap/jig build
```

The cache can be in front of a remote cache on another machine, given by the `cache/remote` property, or the
`JIG_REMOTE_CACHE` environment variable which overrides it, as `ssh://[<user>@]<host>[:<port>][/<jig>]`, which runs `jig serve --stdio` there by ssh, in batch mode, so that it
fails rather than asking for a password, with the `jig` given after the host, or the one in `PATH` there. What is not
in the local cache is looked for in the remote one, and fetched from it into the local one. With `cache/push` set to
`true`, or `JIG_REMOTE_CACHE_PUSH=1`, what is built is pushed to the remote cache too, if it takes pushes, as `jig serve --stdio
--push` does, so that a machine which is trusted, such as CI, fills it, and others only read from it. ssh
authenticates and encrypts the connection, so a key in `authorized_keys` can be limited to reading the cache with
`command="jig serve --stdio"`, or allowed to push with `--push`. A remote cache which fails, as it cannot be reached,
is warned of, and the build goes on with the local cache. `JIG_REMOTE_CACHE` can also be a command, split at spaces,
which serves the cache on its stdin and stdout, such as `ssh -i key cache jig serve --stdio`.

A remote cache can also be a server of files over HTTP, `http://[<user>:<password>@]<host>[:<port>][/<path>]`, which
serves `GET` and `HEAD`, and `PUT` to push to, such as nginx with WebDAV. The files are laid out as a cache's directory
is, so a server of files can serve a jig cache's directory as it is. A user and password in the URL are sent by HTTP's
Basic scheme, and `cache/token-file` as a bearer token. Only `http://` is spoken, so a cache which is not on the
machine or a trusted network is put behind a proxy which speaks TLS to it.

On a private network, a remote cache can instead be served over TCP, by `jig serve --listen <address> --token-file
<path> [--push]`, and reached as `tcp://<host>:<port>`, with the `cache/token-file` property giving the file of the
same token. A client proves it has the token by the hash of it and a random nonce the server sends, so the token is
not sent, but the connection is not encrypted, so it is for networks which are trusted, while ssh both authenticates
and encrypts. The token is read from its file when the cache is connected to, so it is not recorded with the
project. As the remote cache is
part of how a project is configured, changing it configures the project again.

```
JIG_REMOTE_CACHE=ssh://cache.example ./build/bootstrap/jig build
JIG_REMOTE_CACHE=ssh://ci@cache.example JIG_REMOTE_CACHE_PUSH=1 ./build/bootstrap/jig build
```
