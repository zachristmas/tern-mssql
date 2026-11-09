//! Pane to pane messages. The explorer pane asks an editor pane to open some
//! SQL; editors listen on a loopback port and record it in a registry file
//! next to `profiles.json`. A request carries the editor's token, so only
//! this user's explorer (which reads the registry) can talk to it.

use std::{
	hash::{BuildHasher, Hasher},
	io::{BufRead, BufReader, Write},
	net::{TcpListener, TcpStream},
	path::PathBuf,
	sync::mpsc::{self, Receiver},
	time::Duration,
};

use serde::{Deserialize, Serialize};

/// SQL to open in an editor pane.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Request {
	/// The database the SQL is for.
	/// The connection to open it on (an idle pane has none yet).
	pub profile: String,
	pub db:  String,
	/// The text of the new buffer (may be empty).
	pub sql: String,
	/// Run it on arrival.
	pub run: bool,
	/// Only set the database; open no buffer.
	pub quiet: bool,
}

/// What goes over the wire.
#[derive(Deserialize, Serialize)]
struct Wire {
	token: String,
	#[serde(flatten)]
	req:   Request,
}

/// One editor pane's registry entry.
#[derive(Deserialize, Serialize)]
struct Entry {
	profile: String,
	port:    u16,
	token:   String,
	/// The pane's id, so the explorer can focus it.
	pane:    Option<String>,
}

/// Where registry entries and pending SQL files live.
fn dir() -> PathBuf {
	// Tests point this at a temporary folder so they never touch (or message) real panes.
	if let Some(d) = std::env::var_os("TERN_MSSQL_EDITORS_DIR") {
		return PathBuf::from(d);
	}
	crate::profile::path().parent().map(PathBuf::from).unwrap_or_default().join("editors")
}

/// A random hex token.
fn token() -> String {
	let mut out = String::new();
	for i in 0..2u64 {
		let mut h = std::collections::hash_map::RandomState::new().build_hasher();
		h.write_u64(i ^ u64::from(std::process::id()));
		out.push_str(&format!("{:016x}", h.finish()));
	}
	out
}

/// The registry name of a pane that is waiting to become an editor for whichever connection asks first.
pub const IDLE: &str = "*";

/// An editor's side: the registry entry lives as long as this value.
pub struct Listening {
	file: PathBuf,
}

impl Drop for Listening {
	fn drop(&mut self) {
		let _ = std::fs::remove_file(&self.file);
	}
}

/// Starts listening for requests for `profile`.
pub fn listen(profile: &str) -> anyhow::Result<(Listening, Receiver<Request>)> {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();
	let token = token();
	let dir = dir();
	std::fs::create_dir_all(&dir)?;
	let safe: String = profile.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
	let file = dir.join(format!("{safe}-{port}.json"));
	let entry = Entry { profile: profile.to_owned(), port, token: token.clone(), pane: std::env::var("TERN_PANE").ok() };
	std::fs::write(&file, serde_json::to_string(&entry)?)?;
	let (tx, rx) = mpsc::channel();
	std::thread::spawn(move || {
		for stream in listener.incoming().flatten() {
			let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
			let mut line = String::new();
			if BufReader::new(stream).read_line(&mut line).is_ok()
				&& let Ok(w) = serde_json::from_str::<Wire>(&line)
				&& w.token == token
				&& tx.send(w.req).is_err()
			{
				break;
			}
		}
	});
	Ok((Listening { file }, rx))
}

/// Sends `req` to a live editor for `profile`. Returns that editor's pane id on success; dead entries are removed.
fn send(profile: &str, req: &Request) -> Option<Option<String>> {
	// A listening editor for this connection first, then an idle pane (`*`) that can become one.
	let files: Vec<PathBuf> = std::fs::read_dir(dir()).ok()?.flatten().map(|e| e.path()).collect();
	for wanted in [profile, IDLE] {
	for path in &files {
		let Some(entry) = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str::<Entry>(&t).ok()) else { continue };
		if entry.profile != wanted {
			continue;
		}
		let addr = std::net::SocketAddr::from(([127, 0, 0, 1], entry.port));
		match TcpStream::connect_timeout(&addr, Duration::from_millis(300)) {
			Ok(mut s) => {
				let wire = Wire { token: entry.token, req: Request { profile: profile.to_owned(), ..req.clone() } };
				if writeln!(s, "{}", serde_json::to_string(&wire).ok()?).is_ok() {
					return Some(entry.pane);
				}
			},
			Err(_) => {
				let _ = std::fs::remove_file(path);
			},
		}
	}
	}
	None
}

/// Runs `tern ARGS` and returns its stdout when it succeeds.
fn tern(args: &[&str]) -> Option<String> {
	let out = std::process::Command::new("tern").args(args).output().ok()?;
	out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The pane of an editor that is listening right now and sits in the same tab as `here`, whichever connection it serves.
fn editor_pane_beside(here: &str) -> Option<String> {
	let ls = tern(&["ls", "--json"])?;
	let v: serde_json::Value = serde_json::from_str(&ls).ok()?;
	let tab_of = |pane: &str| -> Option<usize> {
		let id: u64 = pane.parse().ok()?;
		let mut n = 0;
		for s in v.get("sessions")?.as_array()? {
			for t in s.get("tabs")?.as_array()? {
				n += 1;
				if t.get("blocks")?.as_array()?.iter().any(|b| b.get("id").and_then(serde_json::Value::as_u64) == Some(id)) {
					return Some(n);
				}
			}
		}
		None
	};
	let tab = tab_of(here)?;
	let files: Vec<PathBuf> = std::fs::read_dir(dir()).ok()?.flatten().map(|e| e.path()).collect();
	for path in files {
		let Some(entry) = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<Entry>(&t).ok()) else { continue };
		let Some(pane) = entry.pane else { continue };
		if tab_of(&pane) != Some(tab) {
			continue;
		}
		let addr = std::net::SocketAddr::from(([127, 0, 0, 1], entry.port));
		if TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok() {
			return Some(pane);
		}
		let _ = std::fs::remove_file(&path);
	}
	None
}

/// Opens `req` in an editor pane for `profile`: an existing one if it is listening, else a new pane. The first editor
/// goes to the right of the explorer; later ones stack under an editor already open in this tab, so the explorer keeps its column.
pub fn open(profile: &str, req: &Request, focus: bool) -> Result<(), String> {
	if let Some(pane) = send(profile, req) {
		if let Some(p) = pane.filter(|_| focus) {
			let _ = tern(&["focus", &p]);
		}
		return Ok(());
	}
	let here = std::env::var("TERN_PANE").map_err(|_| "not running inside a Tern pane".to_owned())?;
	let exe = std::env::current_exe().map_err(|e| e.to_string())?;
	let (anchor, side) = match editor_pane_beside(&here) {
		Some(p) => (p, "down"),
		None => (here, "right"),
	};
	let mut args: Vec<String> = ["split", anchor.as_str(), side, "--json", "--", &exe.to_string_lossy(), "editor", profile, "--db", &req.db]
		.iter()
		.map(ToString::to_string)
		.collect();
	if !req.sql.is_empty() {
		let dir = dir();
		std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
		let file = dir.join(format!("pending-{}.sql", token()));
		std::fs::write(&file, &req.sql).map_err(|e| e.to_string())?;
		args.extend(["--sql-file".to_owned(), file.to_string_lossy().into_owned()]);
		if req.run {
			args.push("--run".to_owned());
		}
	}
	if req.quiet {
		args.push("--quiet".to_owned());
	}
	let refs: Vec<&str> = args.iter().map(String::as_str).collect();
	let out = tern(&refs).ok_or_else(|| "tern split failed".to_owned())?;
	if focus && let Some(block) = serde_json::from_str::<serde_json::Value>(&out).ok().and_then(|v| v.get("block").map(ToString::to_string)) {
		let _ = tern(&["focus", block.trim_matches('"')]);
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn wire_round_trip() {
		let w = Wire { token: "t".into(), req: Request { db: "master".into(), sql: "SELECT 1".into(), run: true, ..Request::default() } };
		let back: Wire = serde_json::from_str(&serde_json::to_string(&w).unwrap()).unwrap();
		assert_eq!((back.token.as_str(), back.req.db.as_str(), back.req.run), ("t", "master", true));
	}

	#[test]
	fn editor_receives_what_the_explorer_sends() {
		let tmp = std::env::temp_dir().join(format!("tern-mssql-test-{}", std::process::id()));
		// Nothing else in the test process reads this variable.
		unsafe { std::env::set_var("TERN_MSSQL_EDITORS_DIR", &tmp) };
		let (_guard, rx) = listen("unit-test-profile").unwrap();
		let sent = send("unit-test-profile", &Request { db: "AppDb".into(), sql: "SELECT 1".into(), run: true, ..Request::default() });
		assert!(sent.is_some());
		let got = rx.recv_timeout(Duration::from_secs(3)).unwrap();
		assert_eq!((got.db.as_str(), got.sql.as_str(), got.run), ("AppDb", "SELECT 1", true));
		// Another profile's request finds no editor.
		assert!(send("some-other-profile", &Request::default()).is_none());
		drop(_guard);
		let _ = std::fs::remove_dir_all(&tmp);
	}

	#[test]
	fn tokens_differ() {
		assert_ne!(token(), token());
		assert_eq!(token().len(), 32);
	}
}
