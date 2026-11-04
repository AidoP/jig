//! Running the actions of cached edges on other machines, workers, which a build sends them to once its own jobs are
//! full, as `jig worker --stdio` serves them over ssh.
//!
//! A worker is a server of [`crate::remote`]'s protocol which also takes `EXECUTE`, with a [`Request`]: the identity of
//! the toolchain the action runs with, the action, with the build's directory replaced by a marker, the environment
//! variables it is given, the names of those it inherits from the worker, which locate its tools there, the blobs of
//! its inputs, by their paths relative to the build's directory, and the paths of the outputs it writes. The client
//! asks first which blobs the worker is missing, and sends only those.
//!
//! The worker runs the action in a directory of its own under its [`Executor::root`]: its inputs are linked there from
//! its cache, at the paths they have relative to the build's directory, which is as deep in it as the paths leave the
//! build's directory with `..`, and the marker is replaced by it. Since the paths a toolchain writes are relative, and
//! remapped, what it writes is what it would write in the client's build. The outputs which are written are added to
//! the worker's cache, and the reply has their blobs, and what the action printed, as a cache's result does, which the
//! client fetches, and restores, as it does a result from a remote cache. The directory is removed once the action has
//! finished.
//!
//! An action reads only what is sent, beside the toolchain, which the worker has, by its identity: one it does not
//! have is refused, with `NOT_FOUND`. An action which needs a file that was not sent fails, as do those whose paths are
//! absolute, or do not lead from the build's directory as given, which are refused, and the client runs it itself.

use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::cache::{Cache, Entry, Hash, Output};
use crate::graph::{Action, Exec};
use crate::remote::{Payload, push_str, push_strs};

/// A toolchain a worker has, by the identity a cached edge has in the graph, with the variables an action which has it
/// is given to run it, such as `RUSTUP_TOOLCHAIN`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toolchain {
    pub identity: String,
    pub env: Vec<(String, String)>,
}

/// What a worker runs actions with: the directory of the directories they run in, which is on the same file system as
/// its cache, so that their inputs are linked from it, the number which run at once, and its toolchains, which are
/// found when an action is first run, so that a client which connects is answered at once.
pub struct Executor {
    pub root: PathBuf,
    pub jobs: usize,
    toolchains: std::sync::OnceLock<Vec<Toolchain>>,
    find: Box<dyn Fn() -> Vec<Toolchain> + Send + Sync>,
}

/// An input of an action which is sent to a worker: its path relative to the build's directory, its blob, and whether
/// it is executable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Input {
    pub path: String,
    pub blob: Hash,
    pub executable: bool,
}

/// An action for a worker to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// The identity of the toolchain it runs with.
    pub identity: String,
    /// The action, with the build's directory replaced by a marker, as [`crate::plan::relocated_action_bytes`] does.
    pub action: Action,
    /// The variables it is given, beside those of its toolchain and its action.
    pub env: Vec<(String, String)>,
    /// The variables it inherits from the worker, which locate the tools and files it uses there.
    pub locating: Vec<String>,
    pub inputs: Vec<Input>,
    /// The files it writes, relative to the build's directory, which are sent back, if it writes them.
    pub outputs: Vec<String>,
}

/// What an action did on a worker: whether it succeeded, and its outputs and what it printed, as the blobs of a
/// result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Executed {
    pub succeeded: bool,
    pub entry: Entry,
}

/// Why a worker did not run an action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    /// It does not have the toolchain.
    Toolchain,
    /// It could not run it, for this reason, such as a path which does not lead from the build's directory.
    Error(String),
}

impl Request {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        push_str(&mut bytes, &self.identity);
        match &self.action {
            Action::Shell(line) => {
                bytes.push(0);
                push_str(&mut bytes, line);
            },
            Action::Exec(exec) => {
                bytes.push(1);
                push_str(&mut bytes, &exec.program);
                push_strs(&mut bytes, &exec.args);
                push_pairs(&mut bytes, &exec.env);
                for optional in [&exec.dir, &exec.stderr] {
                    push_strs(&mut bytes, optional.as_slice());
                }
            },
        }
        push_pairs(&mut bytes, &self.env);
        push_strs(&mut bytes, &self.locating);
        bytes.extend_from_slice(&(self.inputs.len() as u32).to_le_bytes());
        for input in &self.inputs {
            push_str(&mut bytes, &input.path);
            bytes.extend_from_slice(&input.blob);
            bytes.push(u8::from(input.executable));
        }
        push_strs(&mut bytes, &self.outputs);
        bytes
    }
    pub(crate) fn decode(bytes: &[u8]) -> Option<Self> {
        let mut p = Payload(bytes);
        let identity = p.str()?;
        let action = match p.take(1)?[0] {
            0 => Action::Shell(p.str()?),
            1 => {
                let (program, args, env) = (p.str()?, p.strs()?, pairs(&mut p)?);
                let mut optional = || p.strs().filter(|strings| strings.len() <= 1).map(|mut strings| strings.pop());
                let (dir, stderr) = (optional()?, optional()?);
                Action::Exec(Exec { program, args, env, dir, stderr })
            },
            _ => return None,
        };
        let (env, locating) = (pairs(&mut p)?, p.strs()?);
        let inputs = (0..p.u32()?).map(|_| {
            let (path, blob) = (p.str()?, p.hash()?);
            let executable = match p.take(1)?[0] {
                0 => false,
                1 => true,
                _ => return None,
            };
            Some(Input { path, blob, executable })
        });
        let inputs = inputs.collect::<Option<Vec<_>>>()?;
        let outputs = p.strs()?;
        p.0.is_empty().then_some(Self { identity, action, env, locating, inputs, outputs })
    }
    /// The blobs of its inputs, each once.
    #[must_use]
    pub fn blobs(&self) -> Vec<Hash> {
        let mut blobs: Vec<Hash> = self.inputs.iter().map(|input| input.blob).collect();
        blobs.sort_unstable();
        blobs.dedup();
        blobs
    }
}

fn push_pairs(bytes: &mut Vec<u8>, pairs: &[(String, String)]) {
    let flat: Vec<String> = pairs.iter().flat_map(|(name, value)| [name.clone(), value.clone()]).collect();
    push_strs(bytes, &flat);
}

fn pairs(p: &mut Payload) -> Option<Vec<(String, String)>> {
    let mut flat = p.strs()?.into_iter();
    let mut pairs = Vec::new();
    while let Some(name) = flat.next() {
        pairs.push((name, flat.next()?));
    }
    Some(pairs)
}

impl Executed {
    pub(crate) fn encode(&self) -> Vec<u8> {
        [&[u8::from(self.succeeded)], crate::cache::encode_result(&self.entry).as_slice()].concat()
    }
    pub(crate) fn decode(bytes: &[u8]) -> Option<Self> {
        let (&succeeded, result) = bytes.split_first()?;
        let succeeded = match succeeded {
            0 => false,
            1 => true,
            _ => return None,
        };
        Some(Self { succeeded, entry: crate::cache::decode_result(result)? })
    }
}

/// Returns the number of times a path relative to the build's directory leaves it with `..`, before it leads down,
/// or `None` if it is not such a path: one which is absolute, empty, or has `..` after a name.
#[must_use]
pub fn ups(path: &str) -> Option<usize> {
    let (mut ups, mut names) = (0, 0);
    for component in Path::new(path).components() {
        match component {
            Component::ParentDir if names == 0 => ups += 1,
            Component::Normal(_) => names += 1,
            Component::CurDir => (),
            _ => return None,
        }
    }
    (names > 0).then_some(ups)
}

/// The directories actions are run in, which are numbered so that each the process makes is new.
static SANDBOXES: AtomicU64 = AtomicU64::new(0);

/// A directory an action runs in, which is removed when it is dropped.
struct Sandbox(PathBuf);
impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Executor {
    /// An executor with toolchains, which `find` finds once they are first needed.
    pub fn new(root: PathBuf, jobs: usize, find: impl Fn() -> Vec<Toolchain> + Send + Sync + 'static) -> Self {
        Self { root, jobs, toolchains: std::sync::OnceLock::new(), find: Box::new(find) }
    }
    /// Returns the toolchains, finding them if they have not been.
    pub fn toolchains(&self) -> &[Toolchain] {
        self.toolchains.get_or_init(|| (self.find)())
    }
    /// Run an action, with the blobs of its inputs in the cache, adding the outputs it writes to it.
    ///
    /// # Errors
    ///
    /// If the toolchain is not one of the worker's, a path does not lead from the build's directory, an input is not
    /// in the cache, or the directory to run it in cannot be made.
    pub fn execute(&self, cache: &Cache, request: &Request) -> Result<Executed, Refused> {
        use std::os::unix::fs::PermissionsExt;
        let toolchain = self.toolchains().iter().find(|toolchain| toolchain.identity == request.identity);
        let toolchain = toolchain.ok_or(Refused::Toolchain)?;
        let error = |message: String| Refused::Error(message);
        let inputs = request.inputs.iter().map(|input| input.path.as_str());
        let mut paths: Vec<&str> = inputs.chain(request.outputs.iter().map(String::as_str)).collect();
        if let Action::Exec(Exec { dir: Some(dir), .. }) = &request.action {
            paths.push(dir);
        }
        let mut depth = 0;
        for path in paths {
            let ups = ups(path).ok_or_else(|| error(format!("{path:?} does not lead from the build's directory")))?;
            depth = depth.max(ups);
        }
        let count = SANDBOXES.fetch_add(1, Ordering::Relaxed);
        let sandbox = Sandbox(self.root.join(format!("{}-{count}", std::process::id())));
        let _ = std::fs::remove_dir_all(&sandbox.0);
        // the build's directory is as deep as the paths leave it, in directories which the paths do not name
        let build = (0..depth).fold(sandbox.0.clone(), |dir, level| dir.join(format!(".rig-{level}")));
        let failed = |what: &str, path: &Path, failure: std::io::Error| {
            error(format!("failed to {what} {path:?}: {failure}"))
        };
        std::fs::create_dir_all(&build).map_err(|failure| failed("create", &build, failure))?;
        for input in &request.inputs {
            let path = build.join(&input.path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|failure| failed("create", parent, failure))?;
            }
            cache.link(&input.blob, &path, input.executable).map_err(|failure| failed("write", &path, failure))?;
        }
        for output in &request.outputs {
            if let Some(parent) = build.join(output).parent() {
                std::fs::create_dir_all(parent).map_err(|failure| failed("create", parent, failure))?;
            }
        }
        let action = located(&request.action, &build.display().to_string());
        let mut env = request.env.clone();
        env.extend(toolchain.env.iter().cloned());
        let environment = crate::exec::Environment { makeflags: None, env: &env, inherit: Some(&request.locating) };
        let (outcome, printed) = crate::exec::run_command(&action, &build, false, &environment, &|_| ());
        let mut outputs = Vec::new();
        for path in &request.outputs {
            let full = build.join(path);
            let Some(metadata) = std::fs::metadata(&full).ok().filter(std::fs::Metadata::is_file) else {
                continue;
            };
            let hash = crate::blake3::hash_file(&full).map_err(|failure| failed("read", &full, failure))?;
            cache.put_file(&full, &hash).map_err(|failure| failed("keep", &full, failure))?;
            let executable = metadata.permissions().mode() & 0o111 != 0;
            outputs.push(Output { path: path.clone(), blob: hash, executable });
        }
        let printed = cache.put_bytes(&printed);
        let printed = printed.map_err(|failure| error(format!("failed to keep the output: {failure}")))?;
        Ok(Executed { succeeded: outcome.succeeded(), entry: Entry { outputs, printed } })
    }
}

/// Returns an action with the marker of the build's directory replaced by a directory.
fn located(action: &Action, dir: &str) -> Action {
    let locate = |string: &str| string.replace(crate::plan::BUILD_DIR, dir);
    match action {
        Action::Shell(line) => Action::Shell(locate(line)),
        Action::Exec(exec) => Action::Exec(Exec {
            program: locate(&exec.program),
            args: exec.args.iter().map(|arg| locate(arg)).collect(),
            env: exec.env.iter().map(|(name, value)| (name.clone(), locate(value))).collect(),
            dir: exec.dir.as_deref().map(locate),
            stderr: exec.stderr.as_deref().map(locate),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rig-worker-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn executor(dir: &Path) -> Executor {
        let toolchain = Toolchain { identity: "toolchain".into(), env: vec![("TOOLCHAIN".into(), "chosen".into())] };
        Executor::new(dir.join("work"), 2, move || vec![toolchain.clone()])
    }

    fn input(cache: &Cache, path: &str, contents: &str) -> Input {
        Input { path: path.into(), blob: cache.put_bytes(contents.as_bytes()).unwrap(), executable: false }
    }

    #[test]
    fn requests_round_trip() {
        let exec = Exec {
            program: "rustc".into(),
            args: vec!["--out-dir".into(), format!("{}/out", crate::plan::BUILD_DIR)],
            env: vec![("A".into(), "1".into())],
            dir: Some("sub".into()),
            stderr: None,
        };
        let blob = crate::blake3::hash(b"input");
        let request = Request {
            identity: "toolchain".into(),
            action: Action::Exec(exec),
            env: vec![("B".into(), "2".into())],
            locating: vec!["PATH".into()],
            inputs: vec![Input { path: "../src/lib.rs".into(), blob, executable: true }],
            outputs: vec!["out/lib.rlib".into()],
        };
        assert_eq!(Request::decode(&request.encode()), Some(request.clone()));
        let shell = Request { action: Action::Shell("true".into()), ..request };
        assert_eq!(Request::decode(&shell.encode()), Some(shell.clone()));
        assert_eq!(Request::decode(&shell.encode()[1..]), None);
        let entry = Entry { outputs: vec![Output { path: "out".into(), blob, executable: false }], printed: blob };
        let executed = Executed { succeeded: true, entry };
        assert_eq!(Executed::decode(&executed.encode()), Some(executed));
    }

    #[test]
    fn finds_how_paths_leave_the_build_directory() {
        assert_eq!(ups("out/lib.rlib"), Some(0));
        assert_eq!(ups("../src/lib.rs"), Some(1));
        assert_eq!(ups("../../other/src/lib.rs"), Some(2));
        assert_eq!(ups("./out"), Some(0));
        for path in ["/etc/passwd", "", "..", "src/../../x", "../"] {
            assert_eq!(ups(path), None, "{path:?}");
        }
    }

    #[test]
    fn runs_actions_with_their_inputs() {
        let dir = dir("runs");
        let cache = Cache::open(&dir.join("cache")).unwrap();
        let executor = executor(&dir);
        // the build's directory is a directory down, so that `../src` is beside it, and the marker is replaced by it
        let line = format!(
            "cat ../src/a.txt sub/b.txt > out/joined && echo $TOOLCHAIN $GIVEN && test -d {}/sub && echo printed >&2",
            crate::plan::BUILD_DIR,
        );
        let request = Request {
            identity: "toolchain".into(),
            action: Action::Shell(line),
            env: vec![("GIVEN".into(), "given".into())],
            locating: vec!["PATH".into()],
            inputs: vec![input(&cache, "../src/a.txt", "a\n"), input(&cache, "sub/b.txt", "b\n")],
            outputs: vec!["out/joined".into(), "out/unwritten".into()],
        };
        let executed = executor.execute(&cache, &request).unwrap();
        assert!(executed.succeeded);
        let joined = crate::blake3::hash(b"a\nb\n");
        assert_eq!(executed.entry.outputs, vec![Output { path: "out/joined".into(), blob: joined, executable: false }]);
        assert_eq!(cache.read_blob(&executed.entry.printed).unwrap(), b"chosen given\nprinted\n");
        assert!(cache.has_blob(&joined));
        // the directory it ran in is removed, and the inputs, which were linked, are still in the cache
        assert_eq!(std::fs::read_dir(dir.join("work")).unwrap().count(), 0);
        assert_eq!(cache.read_blob(&request.inputs[0].blob).unwrap(), b"a\n");
        // an action which fails is sent back with what it printed
        let failing = Request { action: Action::Shell("echo no; exit 3".into()), ..request.clone() };
        let executed = executor.execute(&cache, &failing).unwrap();
        assert!(!executed.succeeded);
        assert_eq!(cache.read_blob(&executed.entry.printed).unwrap(), b"no\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_what_it_cannot_run() {
        let dir = dir("refuses");
        let cache = Cache::open(&dir.join("cache")).unwrap();
        let executor = executor(&dir);
        let request = Request {
            identity: "toolchain".into(),
            action: Action::Shell("true".into()),
            env: Vec::new(),
            locating: Vec::new(),
            inputs: Vec::new(),
            outputs: vec!["out".into()],
        };
        let other = Request { identity: "other".into(), ..request.clone() };
        assert_eq!(executor.execute(&cache, &other), Err(Refused::Toolchain));
        for path in ["/etc/passwd", "src/../../escape"] {
            let outside = Request { outputs: vec![path.into()], ..request.clone() };
            assert!(matches!(executor.execute(&cache, &outside), Err(Refused::Error(_))), "{path}");
        }
        let blob = crate::blake3::hash(b"not in the cache");
        let missing = Request { inputs: vec![Input { path: "a".into(), blob, executable: false }], ..request };
        assert!(matches!(executor.execute(&cache, &missing), Err(Refused::Error(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
