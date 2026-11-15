# Cleaning up the docs

jig's documentation was a README of a thousand lines and two design documents. The README held how to install jig,
how to develop it, every command, the cache and workers, and the whole reference of the configuration language, in one
page. `docs/build-engine.md` held rig's design, but beneath it, the record of the phases which built it, which is most
of it, and which still describes ninja. Nothing described the files jig keeps, such as `.jig`, or when a project is
configured again.

This plan makes the documentation a book, built with mdBook, which CI builds for every pull request and publishes to
GitHub Pages from main, and then makes what it says current, complete and checked.

## Decided

- **mdBook.** The sources stay Markdown, as they were, so moving them is mostly splitting them. mdBook is one binary,
  as the Rust Book and Cargo's documentation are built with, and its site has search, navigation and highlighting.
- **The book is in `docs/`.** `docs/book.toml` configures it, and its pages are in `docs/src/`, by the order of
  `docs/src/SUMMARY.md`. It is built into the build directory, `build/book`, so that nothing it writes is tracked.
- **mdBook's version is pinned,** as `0.5.4`, in CI and in the plan, so that the site does not change with mdBook's
  releases. It is changed on purpose, as the toolchains are.
- **The README stays short.** It says what jig is, how to bootstrap and install it, and links to the book, as a release
  ships it beside `jig`.
- **No PDF.** The book is a website only.
- **A plan is removed once it is done.** What it made is described in the rest of the book, and its history is in
  git. So `build-engine.md`, of the phases which built rig, and `remove-ninja.md` are gone, and this plan goes when it
  is done.

## The book's layout

- **Introduction.**
- **Using jig:** installing, the commands, toolchains, caching, workers, and the user's configuration.
- **The configuration language:** an overview, then statements, meta-statements, expressions, functions, properties,
  crates, codegen options and profiles, custom commands, generated files, targets, builtin properties and diagnostics.
- **rig:** rig as a library, and rig's design: the graph, the state and how changes are found, environment variables,
  running a build, the cache, workers, and why rig is as it is.
- **Developing jig:** building and testing jig itself, its tests on Windows, and the plans under way.

## Steps

1. **Done: the book, from the README.**
   - `docs/book.toml` and `docs/src/SUMMARY.md` set up the book in the layout above.
   - The README's sections are moved into the book's pages as they are written, with only the references which now lead
     to another page made links, such as "see below". Its headings become the pages' titles.
   - `build-engine.md` and `remove-ninja.md` move into the book as they are, with this plan, so that the links to them
     work on the site.
   - The README keeps what jig is, how to bootstrap, build and install it, and links to the book's pages.
   - Check: `mdbook build docs` succeeds without warnings, and every line of the old README is in a page.
   - **Since:** the README's 1054 lines are 27 pages. Every line of its body is in exactly one of them, as checked
     when it was split, but for its headings, which became titles, and its sentence of what building needs, which is
     split between installing and developing. Each relative link and heading the site's pages link to exists. The
     README lists where each of its sections went. `docs/book.toml` builds the book into `build/book`.
2. **Done: rig's design as it is.**
   - What `build-engine.md` said of rig as it is now, and what its phases built, are the design chapter, in the present
     tense, in `docs/src/rig/`: an overview, with rig's goals, principles, architecture and the version of each format,
     then the graph, the state and how changes are found, environment variables, running a build, the cache, workers,
     and why rig is as it is.
   - The design describes what changed since it was written: edges' inputs, dependencies and what they are built after,
     commands as programs without a shell, no JSON event stream, the toolchain's identity of the channel's manifest
     alone, and the version of each format: the graph's (10), the state log's (4), the cache's (2), the protocol's (11)
     and jig's database (30).
   - Its decisions and risks, and what was decided against, as io_uring, a daemon and bazel-remote, with the
     measurements which decided them, are a page of why rig is as it is.
   - `build-engine.md` and `remove-ninja.md` are removed, as both are done. The measurements and test lists of each
     phase, and how ninja was replaced, are in git's history.
   - Check: `mdbook build docs` succeeds without warnings, each relative link and heading the site's pages link to
     exists, and nothing links to the removed pages.
3. **Done: what is missing.**
   - A page of the files jig keeps: `.jig` at the project root, with what configuring asked rustc, and in the build
     directory, `graph`, `graph.d`, `.rig` and the cache, with when each is written and read again.
   - When a project is configured again: what `graph.d` lists, the variables it read, the toolchain's identity, and
     configuring in jig's own process.
   - Toolchains: that `+<toolchain>` stays with the project until another is given, or `jig generate` runs without
     one, and what that means for `jig +<toolchain> lint` and the commands after it.
   - Windows: what differs, from programs and paths to the proxy wrappers.
   - A pass over the reference, for the same name for the same thing throughout, and examples where a page has none.
   - **Since:** new pages of the files jig keeps, when a project is configured again, `jig check` and rust-analyzer,
     which weren't described, and Windows, and toolchains rewritten for how a toolchain stays with the project. The
     commands page lists every command, with the global options in the order jig takes them. `jig build`, `jig run`,
     `jig test`, `jig lint`, `declare` and the builtin properties have examples, each configured and run before it
     was added. Writing them found that `jig build -n` failed on every project, as the state it opened was locked by
     the same process, which is fixed, with an integration test.
4. **Checked docs.**
   - A test configures each example of a `Jig` file in the book, marked as a `jig` code block, so that an example
     which no longer configures fails.
   - A test checks that the builtin properties and the diagnostics the book lists are those jig has, and every link
     between the book's pages, and to a heading, leads somewhere.
5. **Built by jig, in CI, and published.**
   - A custom command, `book`, in `Jig` builds the book with mdBook into `build/book`, when `mdbook` is found, and
     `jig build custom/book` builds it.
   - CI installs mdBook at the pinned version and builds the book for every pull request, failing on a warning, and
     uploads it as an artifact, so that a pull request's book can be read before it is merged.
   - A push to main builds the book and publishes it to GitHub Pages, with `actions/upload-pages-artifact` and
     `actions/deploy-pages`, in a job with `pages: write` and `id-token: write`, in the `github-pages` environment. The
     repository's Pages are set to be built by GitHub Actions, once, in its settings.
   - The README links to the published book.

## Risks

- **The site differs from GitHub's view of the Markdown.** mdBook and GitHub render a few things differently, such as
  links to headings. The book is read on the site, and its links are checked as the site has them.
- **A link from outside to the README's sections breaks.** The README keeps a list of where each moved, so that its
  readers find them.
- **Examples which are only fragments.** Some examples are parts of a configuration, which do not configure alone.
  They are left unmarked, or given what they need, so that the test only configures those which can.

## Not in this plan

- A PDF of the book.
- A book for each release. The site is the documentation of main.
- Publishing rig's API documentation, from rustdoc, beside the book.
