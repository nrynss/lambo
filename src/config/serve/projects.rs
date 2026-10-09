//! Stdio session selection: `--session`, then the `[[serve.projects]]` cwd
//! map, then `default_session` (#32 PR 8; design §2.2).
//!
//! A stdio `lambo serve` is spawned by its MCP client with the project
//! directory as its working directory. A client that only has a *global*
//! config (one command line for every project) therefore lands in the
//! project's session through this map without the agent choosing one.
//!
//! # Order
//!
//! 1. `--session X` always wins, unchanged: it keeps its looser rule and
//!    nothing here inspects the working directory or `lambo.toml`.
//! 2. Otherwise the `[[serve.projects]]` entry whose path is the **longest
//!    prefix** of the process's canonical working directory. Prefixes are
//!    compared by path component, so `/w/lambo` covers `/w/lambo/src` but not
//!    `/w/lambo-wt`.
//! 3. Otherwise `default_session`.
//! 4. Otherwise refuse with the text clap printed for a missing `--session`
//!    before the flag became optional ([`SESSION_REQUIRED`]).
//!
//! # Paths, `~` and symlinks
//!
//! An entry's `path` must be absolute or start with `~/` (or be `~`), which
//! [`check_project_path`] enforces when the file is read. `~` expands to
//! `$HOME`; an empty or relative `$HOME` counts as unset (a relative one
//! would resolve against the cwd the client chose). With `$HOME` unset, every
//! `~` entry is skipped and the rest of the map still applies, the same
//! degrade-and-say-so as an unresolvable working directory: the result
//! carries [`SelectedSession::tilde_entries_skipped`] (or
//! [`MissingSession::tilde_entries_skipped`]) so the caller warns once.
//! `~user` is refused, because resolving another account's home is
//! not something a config file should make a process do implicitly.
//!
//! Both sides are canonicalized with [`std::fs::canonicalize`] before they are
//! compared: every symlink, `.` and `..` is resolved to the real directory.
//! So entering a project through a symlink selects the entry for the
//! directory the link points at, and an entry written through a symlink (for
//! example `/tmp/...` on macOS, which is `/private/tmp/...`) still matches.
//! Canonicalizing only stats the path; nothing is opened or read. An entry
//! whose path does not exist (or cannot be canonicalized) is skipped: an
//! existing canonical working directory cannot be inside it.
//!
//! When the working directory itself cannot be determined or canonicalized
//! (deleted under the process, or a parent without search permission), the
//! map is skipped and selection falls back to `default_session`, as the
//! design says; the fallback is logged by the caller.
//!
//! Two entries that canonicalize to the same directory but name different
//! sessions are refused as ambiguous rather than resolved by file order, so
//! memory never lands in a session by accident.
//!
//! # Refusals
//!
//! Every message names configured values only (an entry's `path` as written,
//! a session name), never the working directory, the value of `$HOME`, or a
//! credential. The skipped-`~` warning names no entry and no value at all. Session names come out as [`AddressedSessionId`]s: they pass
//! the strict addressed-id rule (already enforced on the file by
//! [`ServeConfig::validate`], re-checked here).

use std::path::{Component, Path, PathBuf};

use super::{serve_err, ServeConfig};
use crate::surface::session::{parse_addressed, AddressedSessionId};
use crate::types::LamboError;

/// The refusal for a stdio serve that has no session: no `--session`, no
/// entry covering the working directory, no `default_session`. It is the text
/// clap printed for a missing `--session` before the flag became optional.
pub const SESSION_REQUIRED: &str =
    "the following required arguments were not provided:\n  --session <SESSION>";

/// Where a stdio serve's session came from, for one startup log line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionSource {
    /// `--session`.
    Flag,
    /// The `[[serve.projects]]` entry with this `path`, as written in the file.
    Project {
        /// The entry's `path`, unexpanded.
        path: String,
    },
    /// `[serve] default_session`, because no entry covered the working
    /// directory.
    DefaultSession,
    /// `[serve] default_session`, because the working directory could not be
    /// canonicalized, so the map could not be consulted.
    DefaultSessionCwdUnavailable,
}

/// The session a stdio serve will own and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectedSession {
    /// The session id. A `--session` value is passed through as given; a
    /// mapped or default session has passed the addressed-id rule.
    pub session: String,
    /// Where it came from.
    pub source: SessionSource,
    /// `$HOME` was unset (or not absolute), so the `~` entries in the map
    /// were skipped: the caller warns once.
    pub tilde_entries_skipped: bool,
}

/// Why nothing selected a session, for the hint under [`SESSION_REQUIRED`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MissingSession {
    /// `$HOME` was unset (or not absolute), so the `~` entries were skipped.
    pub tilde_entries_skipped: bool,
}

impl MissingSession {
    /// The one-paragraph hint a stdio refusal adds under
    /// [`SESSION_REQUIRED`]. Names keys and conditions, never a path or the
    /// value of `$HOME`.
    pub fn hint(&self) -> String {
        let mut hint = String::from(
            "(without --session, a stdio serve uses the lambo.toml [[serve.projects]] entry \
             covering its working directory, then [serve] default_session; neither applies here",
        );
        if self.tilde_entries_skipped {
            hint.push_str(
                "; [[serve.projects]] entries starting with `~` were skipped because HOME is \
                 not set to an absolute path",
            );
        }
        hint.push(')');
        hint
    }
}

/// Why no session could be selected.
#[derive(Debug)]
pub enum SessionSelectionError {
    /// Nothing selected a session: render [`SESSION_REQUIRED`] as a usage
    /// error, with [`MissingSession::hint`] under it.
    Missing(MissingSession),
    /// The map could not be applied safely (an ambiguous pair of entries, or
    /// a session name that is not addressable). A configuration error.
    Config(LamboError),
}

impl std::fmt::Display for SessionSelectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(_) => f.write_str(SESSION_REQUIRED),
            Self::Config(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SessionSelectionError {}

/// The file-time check on one entry's `path`: absolute, `~` or `~/...`.
/// Called by [`ServeConfig::validate`]. Quotes the path, which is
/// configuration, not a secret.
pub(super) fn check_project_path(path: &str) -> Result<(), LamboError> {
    if let Some(rest) = path.strip_prefix('~') {
        if rest.is_empty() || rest.starts_with('/') {
            return Ok(());
        }
        return Err(serve_err(format!(
            "[[serve.projects]] path {path:?}: only `~` and `~/...` are expanded (to $HOME); \
             write another account's home directory out in full"
        )));
    }
    if !Path::new(path).is_absolute() {
        return Err(serve_err(format!(
            "[[serve.projects]] path {path:?} must be absolute or start with `~/`: a stdio \
             serve's working directory is chosen by its client, so a relative path has nothing \
             stable to be relative to"
        )));
    }
    Ok(())
}

/// `path` with a leading `~` replaced by `home`. `None` when the path needs a
/// home and there is none. Assumes [`check_project_path`] passed.
fn expand_home(path: &str, home: Option<&Path>) -> Option<PathBuf> {
    match path.strip_prefix('~') {
        None => Some(PathBuf::from(path)),
        Some(rest) => {
            let home = home?;
            let rest = rest.trim_start_matches('/');
            Some(if rest.is_empty() {
                home.to_path_buf()
            } else {
                home.join(rest)
            })
        }
    }
}

/// Components in a canonical path, for "longest prefix".
fn depth(path: &Path) -> usize {
    path.components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .count()
}

/// A session name from the file, through the strict addressed-id rule.
fn addressed_session(
    field: &str,
    value: &str,
) -> Result<AddressedSessionId, SessionSelectionError> {
    parse_addressed(value).map_err(|_| {
        SessionSelectionError::Config(serve_err(format!(
            "{field} {value:?} is not an addressable session id"
        )))
    })
}

impl ServeConfig {
    /// The session a stdio `lambo serve` owns, from the real process: the
    /// working directory from [`std::env::current_dir`] and `$HOME` from the
    /// environment. See [`ServeConfig::select_stdio_session_with`].
    pub fn select_stdio_session(
        &self,
        flag: Option<&str>,
    ) -> Result<SelectedSession, SessionSelectionError> {
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from);
        self.select_stdio_session_with(flag, std::env::current_dir, home.as_deref())
    }

    /// The selection with the working directory and home injected (tests).
    /// `cwd` is called at most once, and only when there is no `flag` and
    /// the map has entries. A `home` that is not absolute counts as unset.
    ///
    /// Order: `flag`, then the longest `[[serve.projects]]` prefix of the
    /// canonical `cwd`, then `default_session`, else
    /// [`SessionSelectionError::Missing`]. See the module docs for `~`,
    /// symlinks and the fallbacks.
    pub fn select_stdio_session_with(
        &self,
        flag: Option<&str>,
        cwd: impl FnOnce() -> std::io::Result<PathBuf>,
        home: Option<&Path>,
    ) -> Result<SelectedSession, SessionSelectionError> {
        // A relative `$HOME` would be resolved against the process cwd, which
        // for a stdio serve is the client's chosen project, so a `~` entry
        // would match wherever the serve was started. Treat it as unset.
        let home = home.filter(|h| h.is_absolute());
        if let Some(session) = flag {
            return Ok(SelectedSession {
                session: session.to_owned(),
                source: SessionSource::Flag,
                tilde_entries_skipped: false,
            });
        }

        let mut cwd_unavailable = false;
        let mut tilde_entries_skipped = false;
        if !self.projects.is_empty() {
            match cwd().and_then(std::fs::canonicalize) {
                Ok(cwd) => {
                    let matched = self.match_project(&cwd, home)?;
                    tilde_entries_skipped = matched.tilde_entries_skipped;
                    if let Some(selected) = matched.selected {
                        return Ok(selected);
                    }
                }
                Err(_) => cwd_unavailable = true,
            }
        }

        let Some(default) = &self.default_session else {
            return Err(SessionSelectionError::Missing(MissingSession {
                tilde_entries_skipped,
            }));
        };
        let id = addressed_session("default_session", default)?;
        Ok(SelectedSession {
            session: id.as_str().to_owned(),
            source: if cwd_unavailable {
                SessionSource::DefaultSessionCwdUnavailable
            } else {
                SessionSource::DefaultSession
            },
            tilde_entries_skipped,
        })
    }

    /// The longest entry covering the canonical `cwd`, if any. A `~` entry
    /// with no `home` is skipped and reported, not refused.
    fn match_project(
        &self,
        cwd: &Path,
        home: Option<&Path>,
    ) -> Result<MapMatch, SessionSelectionError> {
        // (depth, entry index) of every entry covering `cwd`.
        let mut covering: Vec<(usize, usize)> = Vec::new();
        let mut tilde_entries_skipped = false;
        for (i, project) in self.projects.iter().enumerate() {
            let Some(expanded) = expand_home(&project.path, home) else {
                tilde_entries_skipped = true;
                continue;
            };
            // A path that does not exist cannot contain an existing cwd.
            if let Ok(canonical) = std::fs::canonicalize(&expanded)
                && cwd.starts_with(&canonical)
            {
                covering.push((depth(&canonical), i));
            }
        }
        let none = MapMatch {
            selected: None,
            tilde_entries_skipped,
        };
        let Some(longest) = covering.iter().map(|&(d, _)| d).max() else {
            return Ok(none);
        };
        // Entries at the same depth that both cover `cwd` are the same
        // directory: they agree, or the map is ambiguous.
        let mut tied = covering
            .iter()
            .filter(|&&(d, _)| d == longest)
            .map(|&(_, i)| &self.projects[i]);
        let Some(project) = tied.next() else {
            return Ok(none);
        };
        if let Some(other) = tied.find(|p| p.session != project.session) {
            return Err(SessionSelectionError::Config(serve_err(format!(
                "[[serve.projects]] paths {:?} and {:?} are the same directory but name \
                 different sessions ({:?} and {:?}); keep one entry",
                project.path, other.path, project.session, other.session
            ))));
        }
        let id = addressed_session("[[serve.projects]] session", &project.session)?;
        Ok(MapMatch {
            selected: Some(SelectedSession {
                session: id.as_str().to_owned(),
                source: SessionSource::Project {
                    path: project.path.clone(),
                },
                tilde_entries_skipped,
            }),
            tilde_entries_skipped,
        })
    }
}

/// What [`ServeConfig::match_project`] found.
struct MapMatch {
    selected: Option<SelectedSession>,
    tilde_entries_skipped: bool,
}

#[cfg(test)]
mod tests;
