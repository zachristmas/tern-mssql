//! Autosaved editor sessions, so closing a pane and opening it again in the
//! same directory brings back the same buffers (saved or not). One file per
//! launch directory and connection, next to `profiles.json`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One buffer as it was left.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct SavedBuffer {
	pub name:   String,
	/// The file it is saved as, if any.
	pub path:   Option<String>,
	/// The text as it stands, including edits not yet saved to `path`.
	pub text:   String,
	/// Caret, UTF-16.
	pub cursor: usize,
	pub dirty:  bool,
}

/// A whole session.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Saved {
	pub cwd:     String,
	pub profile: String,
	pub active:  usize,
	pub buffers: Vec<SavedBuffer>,
}

impl Saved {
	/// Worth keeping: some buffer has text or a file.
	pub fn worth_keeping(&self) -> bool {
		self.buffers.iter().any(|b| !b.text.trim().is_empty() || b.path.is_some())
	}
}

/// FNV-1a, which (unlike `DefaultHasher`) is the same in every build.
fn fnv(text: &str) -> u64 {
	text.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

/// The session file for the current directory and `profile`.
pub fn path(profile: &str) -> PathBuf {
	let cwd = std::env::current_dir().unwrap_or_default();
	let safe: String = profile.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
	let dir = crate::profile::path().parent().map(PathBuf::from).unwrap_or_default().join("sessions");
	dir.join(format!("{safe}-{:016x}.json", fnv(&cwd.to_string_lossy().to_lowercase())))
}

/// The file naming the environments the explorer had connected in the current directory.
pub fn envs_path() -> PathBuf {
	let cwd = std::env::current_dir().unwrap_or_default();
	let dir = crate::profile::path().parent().map(PathBuf::from).unwrap_or_default().join("sessions");
	dir.join(format!("envs-{:016x}.json", fnv(&cwd.to_string_lossy().to_lowercase())))
}

/// The environments connected last time in this directory.
pub fn load_envs() -> Vec<String> {
	std::fs::read_to_string(envs_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Remembers the connected environments for this directory.
pub fn store_envs(names: &[String]) -> std::io::Result<()> {
	let file = envs_path();
	if let Some(dir) = file.parent() {
		std::fs::create_dir_all(dir)?;
	}
	let tmp = file.with_extension("tmp");
	std::fs::write(&tmp, serde_json::to_string(names).map_err(std::io::Error::other)?)?;
	std::fs::rename(&tmp, &file)
}

/// The saved session, if there is a readable one.
pub fn load(profile: &str) -> Option<Saved> {
	let saved: Saved = serde_json::from_str(&std::fs::read_to_string(path(profile)).ok()?).ok()?;
	saved.worth_keeping().then_some(saved)
}

/// Writes the session through a temporary file so a crash never leaves half of one.
pub fn store(profile: &str, saved: &Saved) -> std::io::Result<()> {
	let file = path(profile);
	if let Some(dir) = file.parent() {
		std::fs::create_dir_all(dir)?;
	}
	let tmp = file.with_extension("tmp");
	std::fs::write(&tmp, serde_json::to_string(saved).map_err(std::io::Error::other)?)?;
	std::fs::rename(&tmp, &file)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_same_directory_and_profile_give_the_same_file() {
		assert_eq!(path("qas"), path("qas"));
		assert_ne!(path("qas"), path("prod"));
		assert_eq!(fnv("a"), fnv("a"));
		assert_ne!(fnv("a"), fnv("b"));
	}

	#[test]
	fn round_trip_and_worth() {
		let s = Saved {
			cwd:     "C:\\work".into(),
			profile: "qas".into(),
			active:  1,
			buffers: vec![SavedBuffer { name: "SQLQuery_1.sql".into(), path: None, text: "SELECT 1".into(), cursor: 8, dirty: false }],
		};
		let back: Saved = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
		assert_eq!(back, s);
		assert!(s.worth_keeping());
		assert!(!Saved { buffers: vec![SavedBuffer::default()], ..Saved::default() }.worth_keeping());
	}
}
