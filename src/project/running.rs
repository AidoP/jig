//! The attributes of the statements which run a command, `env`, `pass-env`, `worker-tag`, `change-detection` and
//! `cache`, which every such statement takes by [`Project::running_attribute`], so that they mean the same for each.

use super::Project;
use super::value::{Data, Operand};

/// The attributes every statement which runs a command takes, but for a test, which takes only `env`, as it is run
/// by `jig test` rather than by rig.
pub const ATTRIBUTES: [&str; 5] = ["env", "pass-env", "worker-tag", "change-detection", "cache"];

/// What the [`ATTRIBUTES`] of a statement give.
#[derive(Clone, Default)]
pub struct Running {
    /// The environment variables the command is run with.
    pub env: Vec<(String, String)>,
    /// The environment variables it inherits, whose values are its inputs.
    pub pass_env: Vec<String>,
    /// The tags a worker must have for rig to send it the command.
    pub worker_tags: Vec<String>,
    /// How rig detects that its files have changed, `timestamp` or `hash`, in place of the default, if it is given.
    pub change_detection: Option<String>,
    /// Whether rig caches what it builds, in place of the default, if it is given.
    pub cache: Option<bool>,
}

impl Running {
    /// Whether rig stamps the command's files by their contents, which is `hashed` unless `change-detection` says.
    pub fn hashed(&self, hashed: bool) -> bool {
        self.change_detection.as_deref().map_or(hashed, |detection| detection == "hash")
    }
}

impl Project {
    /// Take the value of one of the [`ATTRIBUTES`].
    pub(super) fn running_attribute(&mut self, running: &mut Running, name: &str, operand: &Operand) {
        match name {
            "env" => {
                if let Some(variables) = self.env_variables(operand) {
                    running.env.extend(variables);
                }
            },
            "pass-env" => {
                if let Some(names) = self.variable_names(operand) {
                    running.pass_env.extend(names);
                }
            },
            "worker-tag" => {
                if let Some(tags) = self.tags(operand) {
                    running.worker_tags.extend(tags);
                }
            },
            "change-detection" => match &operand.data {
                Data::Str(value) if value == "timestamp" || value == "hash" => {
                    running.change_detection = Some(value.clone());
                },
                _ => self.invalid_operand(operand, "`'timestamp'` or `'hash'`", false),
            },
            "cache" => {
                if let Some(cache) = self.flag(operand) {
                    running.cache = Some(cache);
                }
            },
            _ => unreachable!("not an attribute of a command: {name}"),
        }
    }
}
