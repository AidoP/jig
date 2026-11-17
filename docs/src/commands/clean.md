# jig clean

`jig clean [<target> ...]` removes what the build wrote, so that it is built again: the outputs of the commands of rig's
graph, their depfiles, and the files their stderr is written to, and rig forgets that the commands ran. What the build
directory holds which no command writes is left, as are the graph and what the project's configuration wrote, so the
project need not be configured again. A directory which removing the files leaves empty is removed too.

With targets, it removes what builds them and what they are built from: `jig clean test/app` removes the test harness of
the crate `app`, and the crates it uses, but not the binary `app`. A target is named as for [jig build](build.md), and
one which the graph does not have is an error.

It removes what the graph which was last generated builds, without configuring the project again, so what a command
which the configuration no longer has wrote is left. It shows how many files it removed. With a [cache](../caching.md),
what is cached is restored from it by the next build, rather than run again, unless the cache is cleared too, with `jig
cache clear`.
