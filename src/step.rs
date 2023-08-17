use std::{collections::HashMap, path::PathBuf};

use gix::{
    objs::decode,
    reference::{head_commit, peel},
    revision::walk,
    traverse::commit::ancestors,
};
use inquire::InquireError;
use log::error;
use miette::Diagnostic;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    command, git, issues, releases,
    releases::{changelog, semver::Label, suggested_package_toml},
    state::RunType,
};

/// Each variant describes an action you can take using knope, they are used when defining your
/// [`crate::Workflow`] via whatever config format is being utilized.
#[derive(Deserialize, Debug, Serialize)]
#[serde(tag = "type")]
pub(crate) enum Step {
    /// Search for Jira issues by status and display the list of them in the terminal.
    /// User is allowed to select one issue which will then change the workflow's state to
    /// [`State::IssueSelected`].
    SelectJiraIssue {
        /// Issues with this status in Jira will be listed for the user to select.
        status: String,
    },
    /// Transition a Jira issue to a new status.
    TransitionJiraIssue {
        /// The status to transition the current issue to.
        status: String,
    },
    /// Search for GitHub issues by status and display the list of them in the terminal.
    /// User is allowed to select one issue which will then change the workflow's state to
    /// [`State::IssueSelected`].
    SelectGitHubIssue {
        /// If provided, only issues with this label will be included
        labels: Option<Vec<String>>,
    },
    /// Attempt to parse issue info from the current branch name and change the workflow's state to
    /// [`State::IssueSelected`].
    SelectIssueFromBranch,
    /// Uses the name of the currently selected issue to checkout an existing or create a new
    /// branch for development. If an existing branch is not found, the user will be prompted to
    /// select an existing local branch to base the new branch off of. Remote branches are not
    /// shown.
    SwitchBranches,
    /// Rebase the current branch onto the branch defined by `to`.
    RebaseBranch {
        /// The branch to rebase onto.
        to: String,
    },
    /// Bump the version of the project in any supported formats found using a
    /// [Semantic Versioning](https://semver.org) rule.
    BumpVersion(releases::Rule),
    /// Run a command in your current shell after optionally replacing some variables.
    Command {
        /// The command to run, with any variable keys you wish to replace.
        command: String,
        /// A map of value-to-replace to [Variable][`crate::command::Variable`] to replace
        /// it with.
        variables: Option<HashMap<String, command::Variable>>,
    },
    /// This will look through all commits since the last tag and parse any
    /// [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/) it finds. It will
    /// then bump the project version (depending on the rule determined from the commits) and add
    /// a new Changelog entry using the [Keep A Changelog](https://keepachangelog.com/en/1.0.0/)
    /// format.
    PrepareRelease(PrepareRelease),
    /// This will create a new release on GitHub using the current project version.
    ///
    /// Requires that GitHub details be configured.
    Release,
    /// Create a new change file to be included in the next release.
    ///
    /// This step is interactive and will prompt the user for the information needed to create the
    /// change file. Do not try to run in a non-interactive environment.
    CreateChangeFile,
}

impl Step {
    pub(crate) fn run(self, run_type: RunType) -> Result<RunType, StepError> {
        match self {
            Step::SelectJiraIssue { status } => issues::select_jira_issue(&status, run_type),
            Step::TransitionJiraIssue { status } => {
                issues::transition_jira_issue(&status, run_type)
            }
            Step::SelectGitHubIssue { labels } => {
                issues::select_github_issue(labels.as_deref(), run_type)
            }
            Step::SwitchBranches => git::switch_branches(run_type),
            Step::RebaseBranch { to } => git::rebase_branch(&to, run_type),
            Step::BumpVersion(rule) => releases::bump_version(run_type, &rule),
            Step::Command { command, variables } => {
                command::run_command(run_type, command, variables)
            }
            Step::PrepareRelease(prepare_release) => {
                releases::prepare_release(run_type, &prepare_release)
            }
            Step::SelectIssueFromBranch => git::select_issue_from_current_branch(run_type),
            Step::Release => releases::release(run_type),
            Step::CreateChangeFile => releases::create_change_file(run_type),
        }
    }

    /// Set `prerelease_label` if `self` is `PrepareRelease`.
    pub(crate) fn set_prerelease_label(&mut self, prerelease_label: &str) {
        if let Step::PrepareRelease(prepare_release) = self {
            prepare_release.prerelease_label = Some(Label::from(prerelease_label));
        }
    }
}

#[derive(Debug, Error, Diagnostic)]
pub(super) enum StepError {
    #[error("No issue selected")]
    #[diagnostic(
        code(step::no_issue_selected),
        help("You must call SelectJiraIssue or SelectGitHubIssue before calling this step")
    )]
    NoIssueSelected,
    #[error("Jira is not configured")]
    #[diagnostic(
        code(step::jira_not_configured),
        help("Jira must be configured in order to call this step"),
        url("https://knope-dev.github.io/knope/config/jira.html")
    )]
    JiraNotConfigured,
    #[error("The specified transition name was not found in the Jira project")]
    #[diagnostic(
    code(step::invalid_jira_transition),
    help("The `transition` field in TransitionJiraIssue must correspond to a valid transition in the Jira project"),
    url("https://knope-dev.github.io/knope/config/jira.html")
    )]
    InvalidJiraTransition,
    #[error("GitHub is not configured")]
    #[diagnostic(
        code(step::github_not_configured),
        help("GitHub must be configured in order to call this step"),
        url("https://knope-dev.github.io/knope/config/github.html")
    )]
    GitHubNotConfigured,
    #[error("Could not open configuration path")]
    #[diagnostic(
        code(step::could_not_open_config_path),
        help(
            "Knope attempts to store config in a local config directory, this error may be a \
            permissions issue or may mean you're using an obscure operating system"
        )
    )]
    CouldNotOpenConfigPath,
    #[error("Could not increment pre-release version {0}")]
    #[diagnostic(
        code(step::invalid_pre_release_version),
        help(
            "The pre-release component of a version must be in the format of `-<label>.N` \
            where <label> is a string and `N` is an integer"
        ),
        url("https://knope-dev.github.io/knope/config/step/BumpVersion.html#pre")
    )]
    InvalidPreReleaseVersion(String),
    #[error("No packages are ready to release")]
    #[diagnostic(
        code(step::no_release),
        help("The `PrepareRelease` step will not complete if no commits cause a package's version to be increased."),
        url("https://knope-dev.github.io/knope/config/step/PrepareRelease.html"),
    )]
    NoRelease,
    #[error("Found invalid semantic version {0}")]
    #[diagnostic(
        code(step::invalid_semantic_version),
        help("The version must be a valid Semantic Version"),
        url("https://knope-dev.github.io/knope/config/packages.html#versioned_files")
    )]
    InvalidSemanticVersion(String),
    #[error("Could not determine the current version of the package")]
    #[diagnostic(
        code(step::no_current_version),
        help("The current version of the package could not be determined"),
        url("https://knope-dev.github.io/knope/config/packages.html#versioned_files")
    )]
    NoCurrentVersion,
    #[error("Versioned files within the same package must have the same version. Found {0} which does not match {1}")]
    #[diagnostic(
        code(step::inconsistent_versions),
        help("Manually update all versioned_files to have the correct version"),
        url("https://knope-dev.github.io/knope/config/step/BumpVersion.html")
    )]
    InconsistentVersions(String, String),
    #[error(transparent)]
    Go(#[from] releases::go::Error),
    #[error(transparent)]
    VersionedFile(#[from] releases::versioned_file::Error),
    #[error("Trouble communicating with a remote API")]
    #[diagnostic(
        code(step::api_request_error),
        help(
            "This occurred during a step that requires communicating with a remote API \
             (e.g., GitHub or Jira). The problem could be an invalid authentication token or a \
             network issue."
        )
    )]
    ApiRequestError,
    #[error("Trouble decoding the response from a remote API")]
    #[diagnostic(
        code(step::api_response_error),
        help(
        "This occurred during a step that requires communicating with a remote API \
                 (e.g., GitHub or Jira). If we were unable to decode the response, it's probably a bug."
        )
    )]
    ApiResponseError(#[source] Option<serde_json::Error>),
    #[error(transparent)]
    #[diagnostic(code(step::io))]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Git(#[from] releases::git::Error),
    #[error("Not a Git repo.")]
    #[diagnostic(
        code(step::not_a_git_repo),
        help(
            "We couldn't find a Git repo in the current directory. Maybe you're not running from the project root?"
        )
    )]
    NotAGitRepo,
    #[error("Not on the tip of a Git branch.")]
    #[diagnostic(
        code(step::not_on_a_git_branch),
        help("In order to run this step, you need to be on the very tip of a Git branch.")
    )]
    NotOnAGitBranch,
    #[error("Bad branch name")]
    #[diagnostic(
        code(step::bad_branch_name),
        help("The branch name was not formatted correctly."),
        url("https://knope-dev.github.io/knope/config/step/SelectIssueFromBranch.html")
    )]
    BadGitBranchName,
    #[error("Uncommitted changes")]
    #[diagnostic(
        code(step::uncommitted_changes),
        help("You need to commit your changes before running this step."),
        url("https://knope-dev.github.io/knope/config/step/SwitchBranches.html")
    )]
    UncommittedChanges,
    #[error("Could not complete checkout")]
    #[diagnostic(
        code(step::incomplete_checkout),
        help("Switching branches failed, but HEAD was changed. You probably want to git switch back \
            to the branch you were on."),
    )]
    IncompleteCheckout(#[source] git2::Error),
    #[error("Unknown Git error.")]
    #[diagnostic(
        code(step::git_error),
        help(
        "Something went wrong when interacting with Git that we don't have an explanation for. \
                Maybe try performing the operation manually?"
        )
    )]
    GitError(#[from] Option<git2::Error>),
    #[error("Could not get head commit")]
    #[diagnostic(
        code(step::head_commit_error),
        help("This step requires HEAD to point to a commit—it was not.")
    )]
    HeadCommitError(#[from] head_commit::Error),
    #[error("Something went wrong with Git")]
    #[diagnostic(
        code(step::unknown_git_error),
        help("Something funky happened with Git, please open a GitHub issue so we can diagnose")
    )]
    UnknownGitError,
    #[error("Command returned non-zero exit code")]
    #[diagnostic(
        code(step::command_failed),
        help("The command failed to execute. Try running it manually to get more information.")
    )]
    CommandError(std::process::ExitStatus),
    #[error("Failed to peel tag, could not proceed with processing commits.")]
    #[diagnostic(
        code(step::peel_tag_error),
        help("In order to process commits for a release, we need to peel the tag. If this fails, it may be a bug."),
    )]
    PeelTagError(#[from] peel::Error),
    #[error("Could not walk backwards from HEAD commit")]
    #[diagnostic(
        code(step::walk_backwards_error),
        help("This step requires walking backwards from HEAD to find the previous release commit. If this fails, make sure HEAD is on a branch."),
    )]
    AncestorsError(#[from] ancestors::Error),
    #[error("Could not walk backwards from HEAD commit")]
    #[diagnostic(
        code(step::walk_error),
        help("This step requires walking backwards from HEAD to find the previous release commit. If this fails, make sure HEAD is on a branch without shallow commits."),
    )]
    WalkError(#[from] walk::Error),
    #[error("Could not decode commit")]
    #[diagnostic(
        code(step::decode_commit_error),
        help("This step requires decoding a commit message. If this fails, it may be a bug.")
    )]
    DecodeError(#[from] decode::Error),
    #[error("Failed to get user input")]
    #[diagnostic(
        code(step::user_input_error),
        help("This step requires user input, but no user input was provided. Try running the step again."),
    )]
    UserInput(#[from] InquireError),
    #[error("PrepareRelease needs to occur before this step")]
    #[diagnostic(
        code(step::release_not_prepared),
        help("You must call the PrepareRelease step before this one."),
        url("https://knope-dev.github.io/knope/config/step/PrepareRelease.html")
    )]
    ReleaseNotPrepared,
    #[error("No packages are defined")]
    #[diagnostic(
        code(step::no_defined_packages),
        help("You must define at least one package in the [[packages]] section of knope.toml. {package_suggestion}"),
        url("https://knope-dev.github.io/knope/config/packages.html")
    )]
    NoDefinedPackages { package_suggestion: String },
    #[error("Too many packages defined")]
    #[diagnostic(
        code(step::too_many_packages),
        help("Only one package in [package] is currently supported for this step.")
    )]
    TooManyPackages,
    #[error("Failed to create the file {0}")]
    #[diagnostic(
        code(step::could_not_create_file),
        help("This could be a permissions issue or a file conflict (the file already exists).")
    )]
    CouldNotCreateFile(PathBuf),
    #[error(transparent)]
    #[diagnostic(
        code(step::could_not_read_changeset),
        help(
            "This could be a file-system issue or a problem with the formatting of a change file."
        )
    )]
    CouldNotReadChangeSet(#[from] changesets::LoadingError),
    #[error("Failed to format a date: {0}")]
    #[diagnostic(
        code(step::could_not_format_date),
        help("This is likely a bug, please report it to https://github.com/knope-dev/knope")
    )]
    CouldNotFormatDate(#[from] time::error::Format),
    #[error(transparent)]
    Changelog(#[from] changelog::Error),
    #[error("Could not serialize generated TOML")]
    #[diagnostic(
        code(step::could_not_serialize_toml),
        help("This is a bug, please report it to https://github.com/knope-dev/knope")
    )]
    GeneratedTOML(#[from] toml::ser::Error),
}

impl StepError {
    pub fn no_defined_packages_with_help() -> Self {
        match suggested_package_toml() {
            Ok(suggested_toml) => Self::NoDefinedPackages {
                package_suggestion: suggested_toml,
            },
            Err(e) => e,
        }
    }
}

/// The inner content of a [`Step::PrepareRelease`] step.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct PrepareRelease {
    /// If set, the user wants to create a pre-release version using the selected label.
    pub(crate) prerelease_label: Option<Label>,
}
