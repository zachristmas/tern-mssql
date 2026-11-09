//! The interactive client: an explorer, query buffers and results on one
//! full-screen Tern surface.

use std::{
	collections::{HashMap, HashSet, VecDeque},
	path::PathBuf,
	time::{Duration, Instant},
};

use tern_sdk::{
	Input, Options, Session, SurfaceOptions, nodes,
	ui::{self, Side, Span, Tone, TreeItem, View, html},
	wire::{self, Event, feature},
};

use crate::{
	backend::{Backend, Cancel, Rows},
	catalog::{Catalog, ObjKind},
	complete::{self, Completion, ItemKind},
	clip, guard, remote, script, tsql,
	worker::{Job, Reply, RunResult, Worker},
};

/// Rows kept per result set.
const LIMIT: usize = 1000;
/// Completion rows shown at once.
const POPUP_ROWS: usize = 8;
/// CSS pixels per unit of the cell size Tern reports (measured: a text line is about 15.5px, a character about 8.1px).
const CSS_PER_CELL_H: f64 = 0.78;
const CSS_PER_CELL_W: f64 = 0.806;
/// Rows drawn per result set.
const RENDER_ROWS: usize = 200;
/// Explorer widths the divider cycles through, in CSS pixels.
const SIDES: [f64; 9] = [130.0, 160.0, 200.0, 240.0, 280.0, 330.0, 380.0, 450.0, 540.0];
/// Separates the parts of an object id (database names can hold dots).
const SEP: char = '\u{1f}';

/// What node handlers report.
#[derive(Clone, Debug)]
enum Msg {
	/// A tree row was double-clicked.
	Activate(String),
	/// A tree row's chevron was clicked.
	Fold(String),
	/// A "Script as" item on an object row.
	ScriptRow(String, script::Kind),
	/// A database menu item on a database row.
	DbRow(String, DbAct),
	/// Native editing changed the buffer.
	Edit(wire::Edit),
	/// Undo was asked for.
	Undo,
	/// The editor was clicked.
	Focus,
	/// A completion was chosen from the list.
	Pick(String),
	/// A query tab was chosen.
	Tab(String),
	/// A results tab was chosen.
	ResultTab(String),
	/// A result cell was clicked: row, column.
	Cell(usize, usize),
	/// A column header was clicked.
	Col(usize),
	/// A row number was clicked.
	Row(usize),
	/// Switch the explorer between schema folders and `schema.name` labels.
	GroupToggle,
	/// A header's sort arrow was clicked.
	Sort(usize),
	/// The corner of the grid was clicked.
	GridAll,
	/// An explorer row was selected.
	TreeSel(String),
	/// The new-tab menu item or the + tab was picked.
	NewTab,
	/// A query tab's × or its menu item was picked.
	CloseTab(usize),
	/// A divider button was clicked: step the explorer width by this many sizes.
	Side(i32),
	/// An environments row was selected.
	EnvSel(String),
	/// Connect to an environment, or show its tree when it is connected.
	EnvConnect(String),
	/// Drop an environment's connection.
	EnvDisconnect(String),
	/// Open the connection form in a pane beside this one: a new connection, or the named one to edit.
	EnvForm(Option<String>),
	/// A new empty query on an environment, in its editor pane.
	EnvQuery(String),
}

/// Which part of the client this process is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
	/// Explorer, editor and results in one pane.
	Whole,
	/// The object tree and connections; opens SQL in an editor pane.
	Explorer,
	/// Buffers and results; takes SQL from the explorer.
	Editor,
}

/// A question shown over the editor that takes the keys until it is answered.
enum Prompt {
	/// Asking where to save buffer `index`; `close` closes it afterwards.
	SaveAs { input: String, index: usize, close: bool },
	/// The chosen file exists.
	Overwrite { path: PathBuf, index: usize, close: bool },
	/// Closing buffer `index`, which has unsaved changes.
	Close(usize),
	/// The system's Save dialog is open for buffer `index`.
	Dialog { index: usize, close: bool },
}

/// What an editor pane opens with.
#[derive(Default)]
pub struct Start {
	pub db:  Option<String>,
	pub sql: Option<String>,
	pub run: bool,
	/// Only set the database; open no buffer.
	pub quiet: bool,
}

/// What the explorer's database menu items do.
#[derive(Clone, Copy, Debug)]
enum DbAct {
	/// A new empty buffer on the database.
	NewQuery,
	/// Make it the editor's database.
	Use,
	/// Reload its objects.
	Refresh,
}

/// The selected block of result cells: where it started and where the caret is.
#[derive(Clone, Copy, Debug)]
struct Sel {
	r0: usize,
	c0: usize,
	r1: usize,
	c1: usize,
}

impl Sel {
	/// `(first row, last row, first column, last column)`.
	fn bounds(self) -> (usize, usize, usize, usize) {
		(self.r0.min(self.r1), self.r0.max(self.r1), self.c0.min(self.c1), self.c0.max(self.c1))
	}

	fn rows(self) -> std::ops::RangeInclusive<usize> {
		let (a, b, ..) = self.bounds();
		a..=b
	}

	fn cols(self) -> std::ops::RangeInclusive<usize> {
		let (.., a, b) = self.bounds();
		a..=b
	}

	fn contains(self, r: usize, c: usize) -> bool {
		self.rows().contains(&r) && self.cols().contains(&c)
	}
}

/// One query buffer.
struct Buffer {
	name:        String,
	path:        Option<PathBuf>,
	text:        String,
	/// Caret, UTF-16.
	cursor:      usize,
	/// Snapshots to undo to.
	undo:        Vec<(String, usize)>,
	/// The last edit typed one character, so the next one joins its undo step.
	last_single: bool,
	dirty:       bool,
}

/// The byte offset of a UTF-16 offset.
fn byte_of(s: &str, units: usize) -> usize {
	let mut n = 0;
	for (i, c) in s.char_indices() {
		if n >= units {
			return i;
		}
		n += c.len_utf16();
	}
	s.len()
}

/// UTF-16 length.
fn u16_len(s: &str) -> usize {
	s.chars().map(char::len_utf16).sum()
}

/// Pasted text with every line ending as `\n`: Windows puts `\r\n` on the clipboard, and the buffer counts lines by `\n`.
fn newlines(s: &str) -> String {
	s.replace("\r\n", "\n").replace('\r', "\n")
}

impl Buffer {
	fn new(name: String, text: String) -> Self {
		let cursor = u16_len(&text);
		Self { name, path: None, text, cursor, undo: Vec::new(), last_single: false, dirty: false }
	}

	/// The caret as a byte offset.
	/// Changed since it was saved, or never saved and not empty.
	fn unsaved(&self) -> bool {
		self.dirty || (self.path.is_none() && !self.text.trim().is_empty())
	}

	fn cursor_byte(&self) -> usize {
		byte_of(&self.text, self.cursor)
	}

	/// `(line, column)`, one-based.
	fn line_col(&self) -> (usize, usize) {
		let upto = &self.text[..self.cursor_byte()];
		let line = upto.matches('\n').count() + 1;
		let col = upto.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
		(line, col)
	}

	/// Applies a native edit. False when Tern's text length disagrees with ours.
	fn apply(&mut self, e: &wire::Edit) -> bool {
		self.apply_parts(e.from, e.to, &e.text, e.cursor, e.len)
	}

	/// [`apply`](Self::apply) from the edit's parts: replace `from..to` (UTF-16) with `text`, caret to `cursor`.
	fn apply_parts(&mut self, from: usize, to: usize, text: &str, cursor: usize, len: usize) -> bool {
		if len != u16_len(&self.text) {
			return false;
		}
		// A click in the editor arrives as an empty edit: only the caret moves.
		if from == to && text.is_empty() {
			self.cursor = cursor.min(len);
			return true;
		}
		let single = from == to && u16_len(text) == 1 && text != "
";
		if !(single && self.last_single) {
			self.undo.push((self.text.clone(), self.cursor));
			if self.undo.len() > 500 {
				self.undo.remove(0);
			}
		}
		self.last_single = single;
		let (a, b) = (byte_of(&self.text, from), byte_of(&self.text, to));
		self.text.replace_range(a.min(b)..b.max(a), text);
		self.cursor = cursor;
		self.dirty = true;
		true
	}



	/// Edits from a plain key event, for terminals that don't send native edits. False when the key is not an edit.
	fn key(&mut self, key: &tern_sdk::keys::Key) -> bool {
		let c = self.cursor_byte();
		let single_insert = |s: &str| u16_len(s) == 1 && s != "\n";
		let (mutates, joins) = match key.name.as_str() {
			"enter" => (true, false),
			"tab" => (true, false),
			"backspace" => (c > 0, false),
			"delete" => (c < self.text.len(), false),
			"paste" => (true, false),
			_ => match key.typed() {
				Some(s) if !key.ctrl && !key.meta => (true, single_insert(s)),
				_ => (false, false),
			},
		};
		if mutates && !(joins && self.last_single) {
			self.undo.push((self.text.clone(), self.cursor));
			if self.undo.len() > 500 {
				self.undo.remove(0);
			}
		}
		if mutates {
			self.last_single = joins;
			self.dirty = true;
		}
		match key.name.as_str() {
			"enter" => self.put("\n"),
			"tab" => self.put("  "),
			"paste" => self.put(&newlines(key.text.as_deref().unwrap_or_default())),
			"backspace" if key.ctrl && c > 0 => {
				let from = self.word_before(c);
				self.cursor -= u16_len(&self.text[from..c]);
				self.text.replace_range(from..c, "");
			},
			"delete" if key.ctrl && c < self.text.len() => {
				let to = self.word_after(c);
				self.text.replace_range(c..to, "");
			},
			"left" if key.ctrl => self.cursor -= u16_len(&self.text[self.word_before(c)..c]),
			"right" if key.ctrl => self.cursor += u16_len(&self.text[c..self.word_after(c)]),
			"backspace" if c > 0 => {
				let (i, ch) = self.text[..c].char_indices().next_back().unwrap_or((0, ' '));
				self.text.remove(i);
				self.cursor -= ch.len_utf16();
			},
			"delete" if c < self.text.len() => {
				self.text.remove(c);
			},
			"left" => {
				if let Some((_, ch)) = self.text[..c].char_indices().next_back() {
					self.cursor -= ch.len_utf16();
				}
			},
			"right" => {
				if let Some(ch) = self.text[c..].chars().next() {
					self.cursor += ch.len_utf16();
				}
			},
			"home" => {
				let start = self.text[..c].rfind('\n').map_or(0, |i| i + 1);
				self.cursor -= u16_len(&self.text[start..c]);
			},
			"end" => {
				let end = self.text[c..].find('\n').map_or(self.text.len(), |i| c + i);
				self.cursor += u16_len(&self.text[c..end]);
			},
			"up" | "down" => self.vertical(key.name == "down"),
			_ => match key.typed() {
				Some(s) if !key.ctrl && !key.meta => self.put(s),
				_ => return false,
			},
		}
		true
	}

	/// Where the word before byte `c` starts: skips spaces, then a run of word characters or of punctuation.
	fn word_before(&self, c: usize) -> usize {
		let class = |ch: char| if ch.is_alphanumeric() || ch == '_' { 1 } else if ch == '\n' { 2 } else { 0 };
		let mut i = c;
		let mut chars = self.text[..c].chars().rev().peekable();
		while let Some(&ch) = chars.peek() {
			if ch == ' ' || ch == '\t' {
				i -= ch.len_utf8();
				chars.next();
			} else {
				break;
			}
		}
		let Some(&first) = chars.peek() else { return i };
		let kind = class(first);
		for ch in chars {
			if class(ch) != kind || kind == 2 && i < c && self.text[i..c].contains('\n') {
				break;
			}
			i -= ch.len_utf8();
			if kind == 2 {
				break;
			}
		}
		i
	}

	/// Where the word after byte `c` ends.
	fn word_after(&self, c: usize) -> usize {
		let class = |ch: char| if ch.is_alphanumeric() || ch == '_' { 1 } else if ch == '\n' { 2 } else { 0 };
		let mut i = c;
		let mut chars = self.text[c..].chars().peekable();
		while let Some(&ch) = chars.peek() {
			if ch == ' ' || ch == '\t' {
				i += ch.len_utf8();
				chars.next();
			} else {
				break;
			}
		}
		let Some(&first) = chars.peek() else { return i };
		let kind = class(first);
		for ch in chars {
			if class(ch) != kind {
				break;
			}
			i += ch.len_utf8();
			if kind == 2 {
				break;
			}
		}
		i
	}

	/// Inserts text at the caret.
	fn put(&mut self, s: &str) {
		let c = self.cursor_byte();
		self.text.insert_str(c, s);
		self.cursor += u16_len(s);
	}

	/// Moves the caret one line up or down, keeping its column.
	fn vertical(&mut self, down: bool) {
		let c = self.cursor_byte();
		let line_start = self.text[..c].rfind('\n').map_or(0, |i| i + 1);
		let col = self.text[line_start..c].chars().count();
		let target = if down {
			let Some(nl) = self.text[c..].find('\n') else { return };
			c + nl + 1
		} else {
			if line_start == 0 {
				return;
			}
			self.text[..line_start - 1].rfind('\n').map_or(0, |i| i + 1)
		};
		let line_end = self.text[target..].find('\n').map_or(self.text.len(), |i| target + i);
		let len = self.text[target..line_end].chars().count();
		let at: usize = self.text[target..].chars().take(col.min(len)).map(char::len_utf8).sum();
		self.cursor = u16_len(&self.text[..target + at]);
	}
	/// Undoes the last edit step.
	fn undo(&mut self) {
		if let Some((text, cursor)) = self.undo.pop() {
			self.text = text;
			self.cursor = cursor;
			self.last_single = false;
			self.dirty = true;
		}
	}
}

/// What the results area shows.
#[derive(Default)]
struct Output {
	sets:     Vec<Rows>,
	messages: Vec<String>,
	error:    bool,
	/// Active tab: an index into `sets`, or `sets.len()` for Messages.
	tab:      usize,
	/// The selected cells of the active result set.
	sel:      Option<Sel>,
	/// The active set's rows as they arrived, kept while it is sorted or filtered.
	base:     Option<Vec<crate::view::Row>>,
	/// Sorted column and direction (true is ascending).
	sort:     Option<(usize, bool)>,
	/// Rows must contain this text.
	filter:   String,
	/// Keys are going into the filter.
	typing:   bool,
}

/// A job waiting on or running in the worker.
struct Pending {
	since:  Instant,
	what:   &'static str,
	cancel: Cancel,
	asked:  bool,
}

/// The completion popup.
struct Popup {
	comp: Completion,
	sel:  usize,
}

/// One server connection: its worker thread, databases and loaded catalogs.
struct Conn {
	profile:   String,
	server:    String,
	worker:    Worker,
	/// Jobs in the worker, front first.
	queue:     VecDeque<Pending>,
	databases: Vec<String>,
	/// The database the connection (and so the editor) is in.
	current:   String,
	/// Loaded catalogs by database name.
	catalogs:  HashMap<String, Catalog>,
	/// Databases whose catalog is being loaded.
	loading:   HashSet<String>,
}

impl Conn {
	/// Reads the database list and hands the connection to a worker thread.
	fn new<B: Backend + Send + 'static>(mut db: B, profile: String, server: String) -> anyhow::Result<Self> {
		let databases = db.databases()?;
		let current = db.current()?;
		Ok(Self { profile, server, worker: Worker::spawn(db), queue: VecDeque::new(), databases, current, catalogs: HashMap::new(), loading: HashSet::new() })
	}

	/// Queues a job in the worker and returns the note to show.
	fn start(&mut self, job: Job, what: &'static str) -> String {
		let cancel = self.worker.submit(job);
		self.queue.push_back(Pending { since: Instant::now(), what, cancel, asked: false });
		if self.queue.len() > 1 { format!("{what} queued") } else { format!("{what}... (esc cancels)") }
	}

	/// Starts loading `db`'s catalog unless it is loaded or loading; the note to show when it starts.
	fn ensure_catalog(&mut self, db: &str) -> Option<String> {
		if self.catalogs.contains_key(db) || !self.loading.insert(db.to_owned()) {
			return None;
		}
		Some(self.start(Job::Catalog(db.to_owned()), "loading objects"))
	}
}

/// The tree id of a connection's root row.
fn root_id(profile: &str) -> String {
	format!("srv{SEP}{profile}")
}

/// The tree id of a database row.
fn db_id(profile: &str, db: &str) -> String {
	format!("db:{profile}{SEP}{db}")
}

/// A tree id's kind, profile and the rest: `db:prod␟Titan` is `("db:", "prod", "Titan")`, a root `srv␟prod` has an empty rest.
/// Placeholder rows (`p:` + the row they stand in for) answer for that row.
fn split_id(id: &str) -> Option<(&str, &str, &str)> {
	let id = id.strip_prefix("p:").unwrap_or(id);
	if let Some(rest) = id.strip_prefix("srv") {
		return Some(("srv", rest.strip_prefix(SEP)?, ""));
	}
	let colon = id.find(':')?;
	let (kind, rest) = id.split_at(colon + 1);
	let (profile, tail) = rest.split_once(SEP)?;
	Some((kind, profile, tail))
}

/// The client's state.
struct App {
	/// Connections: an editor has one, an explorer as many as are connected.
	conns:     Vec<Conn>,
	/// Saved profiles, for the explorer's environments box.
	envs:      Vec<crate::profile::Profile>,
	/// The environments row selected.
	env_at:    usize,
	/// Keys go to the environments box rather than the tree.
	in_envs:   bool,
	/// Connections being opened on a thread, by profile name.
	connecting: Vec<(String, std::sync::mpsc::Receiver<anyhow::Result<Conn>>)>,
	/// Why an environment failed to connect, by profile name.
	env_fail:  HashMap<String, String>,
	/// `profiles.json` as last read, so edits made in a form pane show up.
	envs_seen: Option<std::time::SystemTime>,
	buffers:   Vec<Buffer>,
	active:    usize,
	next_id:   usize,
	out:       Output,
	popup:     Option<Popup>,
	/// Tree rows that are open, by id.
	expanded:  HashSet<String>,
	note:      String,
	write:     bool,
	in_editor: bool,
	/// The explorer row last selected, which the right-click menu acts on.
	picked: Option<String>,
	/// The popup position last sent as a stylesheet.
	pop_css: String,
	/// Bumped to give the editor a fresh identity (and our text) when it and we disagree.
	epoch: u32,
	/// Index into `SIDES`.
	side: usize,
	/// Width and height of a character cell, in CSS pixels.
	cell: (f64, f64),
	/// The pane's height in rows, which the layout bounds are shares of.
	rows: f64,
	/// The pane's width in columns, which keeps the completion list inside the editor.
	cols: f64,
	/// Native `edit` events seen; while zero, plain keys edit the buffer.
	native:    usize,
	/// Tern's editor is believed to hold keyboard focus (false after any click elsewhere).
	ed_focused: bool,
	/// Keys go to the results grid (selection, copy) instead of the editor.
	grid_focus: bool,
	role: Role,
	prompt: Option<Prompt>,
	/// The answer of the system Save dialog, while one is open.
	dialog: Option<std::sync::mpsc::Receiver<crate::dialog::Outcome>>,
	/// The session as last written, to skip identical writes.
	saved_json: String,
	/// The explorer groups objects in a folder per schema.
	by_schema: bool,
	/// An editor's listener and the requests it receives.
	remote: Option<(remote::Listening, std::sync::mpsc::Receiver<remote::Request>)>,
}

/// `[schema].[name]`.
fn quoted(schema: &str, name: &str) -> String {
	format!("[{}].[{}]", schema.replace(']', "]]"), name.replace(']', "]]"))
}

/// A styled span.
fn span(t: impl Into<String>, s: &str) -> Span {
	Span { t: t.into(), s: Some(s.to_owned()), fx: None, href: None }
}

/// A table cell with newlines collapsed so rows stay one line tall.
fn flat(cell: &str) -> String {
	cell.replace(['\r', '\n'], " ")
}

impl App {
	/// Loads the server's database list, starts the catalog load and hands the connection to the worker.
	fn new<B: Backend + Send + 'static>(db: B, profile: String, server: String, files: &[PathBuf]) -> anyhow::Result<Self> {
		let conn = Conn::new(db, profile, server)?;
		let current = conn.current.clone();
		let mut buffers: Vec<Buffer> = Vec::new();
		for (i, f) in files.iter().enumerate() {
			let mut b = Buffer::new(
				f.file_name().map_or_else(|| format!("SQLQuery_{}.sql", i + 1), |n| n.to_string_lossy().into_owned()),
				std::fs::read_to_string(f)?,
			);
			b.path = Some(f.clone());
			buffers.push(b);
		}
		if buffers.is_empty() {
			buffers.push(Buffer::new("SQLQuery_1.sql".into(), String::new()));
		}
		let expanded = [root_id(&conn.profile), db_id(&conn.profile, &current), format!("g:{}{SEP}{current}:tables", conn.profile)].into();
		let mut app = Self {
			conns: vec![conn],
			envs: Vec::new(),
			env_at: 0,
			in_envs: false,
			connecting: Vec::new(),
			env_fail: HashMap::new(),
			envs_seen: None,
			next_id: buffers.len() + 1,
			buffers,
			active: 0,
			out: Output::default(),
			popup: None,
			expanded,
			note: String::new(),
			write: false,
			in_editor: true,
			picked: None,
			pop_css: String::new(),
			rows: pane_rows().unwrap_or(40.0),
			cols: pane_dim("cols").unwrap_or(160.0),
			epoch: 0,
			side: 4,
			cell: (10.0, 20.0),
			native: 0,
			ed_focused: true,
			grid_focus: false,
			role: Role::Whole,
			prompt: None,
			dialog: None,
			saved_json: String::new(),
			by_schema: true,
			remote: None,
		};
		app.ensure_catalog(0, &current);
		Ok(app)
	}

	/// The catalog of the database the editor is in.
	fn catalog(&self) -> Option<&Catalog> {
		let c = self.conns.first()?;
		c.catalogs.get(&c.current)
	}

	/// The editor's profile name (empty in an explorer with nothing connected).
	fn profile(&self) -> &str {
		self.conns.first().map_or("", |c| c.profile.as_str())
	}

	/// The database the editor is in (empty in an explorer with nothing connected).
	fn current(&self) -> &str {
		self.conns.first().map_or("", |c| c.current.as_str())
	}

	/// The connection a tree row belongs to.
	fn conn_of(&self, id: &str) -> Option<usize> {
		let (_, profile, _) = split_id(id)?;
		self.conns.iter().position(|c| c.profile == profile)
	}

	/// Starts loading `db`'s catalog on connection `ci` unless it is loaded or loading.
	fn ensure_catalog(&mut self, ci: usize, db: &str) {
		if let Some(note) = self.conns.get_mut(ci).and_then(|c| c.ensure_catalog(db)) {
			self.note = note;
		}
	}

	/// Lines the work area can use: the pane less the status bar.
	fn avail_lines(&self) -> f64 {
		self.rows - 4.0
	}

	/// Puts a paste into the editor: the key's own text (a bracketed paste) or the clipboard (Ctrl+V).
	/// Tern never pastes into the editor itself, and in native-edit mode it keeps its own copy of the text,
	/// so a new editor identity makes it take ours. True when the editor must be given the caret again.
	fn paste(&mut self, key: &tern_sdk::keys::Key) -> bool {
		let text = if key.name == "paste" {
			key.text.clone().unwrap_or_default()
		} else {
			match clip::get() {
				Ok(text) => text,
				Err(e) => {
					self.note = format!("paste failed: {e}");
					return false;
				},
			}
		};
		if text.is_empty() {
			return false;
		}
		let was_focused = self.ed_focused && self.in_editor;
		let paste = tern_sdk::keys::Key { name: "paste".into(), text: Some(text), ..Default::default() };
		self.buf().key(&paste);
		let retake = self.native > 0 && was_focused;
		if retake {
			self.epoch += 1;
			self.popup = None;
		}
		self.in_editor = true;
		self.ed_focused = true;
		self.complete(false);
		retake || !was_focused
	}

	/// Lines the prompt bar takes from the editor.
	fn prompt_lines(&self) -> f64 {
		if self.prompt.is_some() { 2.0 } else { 0.0 }
	}

	/// Lines the editor takes: a fixed share, so the results get the rest.
	fn editor_lines(&self) -> f64 {
		((self.avail_lines() - 5.5) * 0.45).round().max(4.0) - self.prompt_lines()
	}

	/// Lines left for the results grid after both tab strips, the editor and the gap.
	fn body_lines(&self) -> f64 {
		(self.avail_lines() - 5.0 - self.editor_lines() - self.prompt_lines()).max(3.0)
	}

	/// A query tab's label: the environment it runs on, then the buffer, so a script is never run on the wrong server by mistake.
	fn tab_label(&self, b: &Buffer) -> String {
		format!("{} - {}", self.profile().to_uppercase(), b.name)
	}

	/// The editor's node id.
	fn editor_id(&self) -> String {
		format!("main.body.work.ed{}", self.epoch)
	}

	fn buf(&mut self) -> &mut Buffer {
		&mut self.buffers[self.active]
	}

	/// Queues a job in the editor's worker.
	fn start(&mut self, job: Job, what: &'static str) {
		self.start_on(0, job, what);
	}

	/// Queues a job in connection `ci`'s worker.
	fn start_on(&mut self, ci: usize, job: Job, what: &'static str) {
		if let Some(c) = self.conns.get_mut(ci) {
			self.note = c.start(job, what);
		}
	}

	/// Asks the running job to stop.
	fn cancel(&mut self) {
		for c in &mut self.conns {
			if let Some(p) = c.queue.front_mut().filter(|p| !p.asked) {
				p.cancel.cancel();
				p.asked = true;
				self.note = "cancelling...".into();
				return;
			}
		}
	}

	/// Opens a new buffer and makes it active.
	fn new_buffer(&mut self, text: String) {
		let name = format!("SQLQuery_{}.sql", self.next_id);
		self.next_id += 1;
		self.buffers.push(Buffer::new(name, text));
		self.active = self.buffers.len() - 1;
		self.popup = None;
	}

	/// Closes buffer `i`, keeping at least one and the same buffer active where possible.
	fn close_buffer_at(&mut self, i: usize) {
		if i >= self.buffers.len() {
			return;
		}
		if self.buffers.len() > 1 {
			self.buffers.remove(i);
			if i < self.active {
				self.active -= 1;
			}
			self.active = self.active.min(self.buffers.len() - 1);
		} else {
			self.buffers[0] = Buffer::new("SQLQuery_1.sql".into(), String::new());
		}
		self.popup = None;
	}

	/// The buffers as a session.
	fn snapshot(&self) -> crate::session::Saved {
		crate::session::Saved {
			cwd:     std::env::current_dir().unwrap_or_default().display().to_string(),
			profile: self.profile().to_owned(),
			active:  self.active,
			buffers: self
				.buffers
				.iter()
				.map(|b| crate::session::SavedBuffer {
					name:   b.name.clone(),
					path:   b.path.as_ref().map(|p| p.display().to_string()),
					text:   b.text.clone(),
					cursor: b.cursor,
					dirty:  b.dirty,
				})
				.collect(),
		}
	}

	/// Brings back a saved session's buffers.
	fn restore(&mut self, saved: crate::session::Saved) {
		self.buffers = saved
			.buffers
			.into_iter()
			.map(|sb| {
				let mut b = Buffer::new(sb.name, sb.text);
				b.path = sb.path.map(PathBuf::from);
				b.cursor = sb.cursor.min(u16_len(&b.text));
				b.dirty = sb.dirty;
				b
			})
			.collect();
		if self.buffers.is_empty() {
			self.buffers.push(Buffer::new("SQLQuery_1.sql".into(), String::new()));
		}
		self.active = saved.active.min(self.buffers.len() - 1);
		self.next_id = self.buffers.iter().filter_map(|b| b.name.strip_prefix("SQLQuery_")?.strip_suffix(".sql")?.parse::<usize>().ok()).max().unwrap_or(0).max(self.buffers.len()) + 1;
		self.note = format!("restored {} buffer(s) from the last session", self.buffers.len());
	}

	/// Writes the session when it changed.
	fn autosave(&mut self) {
		let snap = self.snapshot();
		if !snap.worth_keeping() {
			// Everything was closed: forget the old session too.
			if !self.saved_json.is_empty() {
				let _ = std::fs::remove_file(crate::session::path(self.profile()));
				self.saved_json.clear();
			}
			return;
		}
		let Ok(json) = serde_json::to_string(&snap) else { return };
		if json != self.saved_json && crate::session::store(self.profile(), &snap).is_ok() {
			self.saved_json = json;
		}
	}

	/// Ctrl+S: saves in place when the buffer has a file, else asks where.
	fn save(&mut self) {
		let i = self.active;
		match self.buffers[i].path.clone() {
			Some(p) => {
				self.write_buffer(i, p, false);
			},
			None => self.ask_where(i, false),
		}
	}

	/// Asks for a path for buffer `i`, starting from its file or the pane's directory.
	fn ask_where(&mut self, i: usize, close: bool) {
		if std::env::var_os("TERN_MSSQL_SAVE_PROMPT").is_none() && crate::dialog::available() {
			let cwd = std::env::current_dir().unwrap_or_default();
			let b = &self.buffers[i];
			let (dir, name) = match &b.path {
				Some(p) => (p.parent().map_or_else(|| cwd.clone(), PathBuf::from), p.file_name().map_or_else(|| b.name.clone(), |n| n.to_string_lossy().into_owned())),
				None => (cwd, b.name.clone()),
			};
			let (tx, rx) = std::sync::mpsc::channel();
			std::thread::spawn(move || {
				let _ = tx.send(crate::dialog::save_as(&dir, &name));
			});
			self.dialog = Some(rx);
			self.prompt = Some(Prompt::Dialog { index: i, close });
			return;
		}
		let b = &self.buffers[i];
		let start = b.path.clone().unwrap_or_else(|| std::env::current_dir().unwrap_or_default().join(&b.name));
		self.prompt = Some(Prompt::SaveAs { input: start.display().to_string(), index: i, close });
	}

	/// Writes buffer `i` to `path`; true when it worked.
	fn write_buffer(&mut self, i: usize, path: PathBuf, close: bool) -> bool {
		match std::fs::write(&path, &self.buffers[i].text) {
			Ok(()) => {
				let b = &mut self.buffers[i];
				b.dirty = false;
				if let Some(n) = path.file_name() {
					b.name = n.to_string_lossy().into_owned();
				}
				self.note = format!("saved {}", path.display());
				b.path = Some(path);
				if close {
					self.close_buffer_at(i);
				}
				true
			},
			Err(e) => {
				self.note = format!("save failed: {e}");
				false
			},
		}
	}

	/// Takes the system Save dialog's answer when it has one. True when something changed.
	fn poll_dialog(&mut self) -> bool {
		let Some(rx) = &self.dialog else { return false };
		let Ok(outcome) = rx.try_recv() else { return false };
		self.dialog = None;
		// Esc may have dismissed the bar already; then the answer is stale.
		let Some(Prompt::Dialog { index, close }) = self.prompt.take_if(|p| matches!(p, Prompt::Dialog { .. })) else { return true };
		match outcome {
			crate::dialog::Outcome::Chosen(path) => {
				if !self.write_buffer(index, path.clone(), close) {
					self.prompt = Some(Prompt::SaveAs { input: path.display().to_string(), index, close });
				}
			},
			crate::dialog::Outcome::Cancelled => self.note = "save cancelled".into(),
			crate::dialog::Outcome::Unavailable => {
				let start = self.buffers.get(index).map(|b| std::env::current_dir().unwrap_or_default().join(&b.name)).unwrap_or_default();
				self.prompt = Some(Prompt::SaveAs { input: start.display().to_string(), index, close });
			},
		}
		true
	}

	/// Closes buffer `i`, asking first when it has unsaved changes.
	fn request_close(&mut self, i: usize) {
		if i >= self.buffers.len() {
			return;
		}
		if self.buffers[i].unsaved() {
			self.active = i;
			self.prompt = Some(Prompt::Close(i));
		} else {
			self.close_buffer_at(i);
		}
	}

	/// A key while a prompt is up.
	fn prompt_key(&mut self, key: &tern_sdk::keys::Key) {
		let Some(prompt) = self.prompt.take() else { return };
		let yes = key.typed().is_some_and(|t| t.eq_ignore_ascii_case("y"));
		let no = key.typed().is_some_and(|t| t.eq_ignore_ascii_case("n")) || key.is("escape");
		self.prompt = match prompt {
			Prompt::SaveAs { mut input, index, close } => {
				if key.is("escape") {
					None
				} else if key.is("enter") {
					let raw = input.trim().trim_matches('"').to_owned();
					if raw.is_empty() {
						Some(Prompt::SaveAs { input, index, close })
					} else {
						let mut p = PathBuf::from(&raw);
						if !p.is_absolute() {
							p = std::env::current_dir().unwrap_or_default().join(p);
						}
						if p.extension().is_none() {
							p.set_extension("sql");
						}
						let same = self.buffers.get(index).and_then(|b| b.path.as_ref()) == Some(&p);
						if p.exists() && !same {
							Some(Prompt::Overwrite { path: p, index, close })
						} else if self.write_buffer(index, p, close) {
							None
						} else {
							Some(Prompt::SaveAs { input, index, close })
						}
					}
				} else {
					let sep = |c: char| matches!(c, '\\' | '/' | ' ');
					if key.ctrl && key.name == "backspace" {
						while input.chars().last().is_some_and(sep) {
							input.pop();
						}
						while input.chars().last().is_some_and(|c| !sep(c)) {
							input.pop();
						}
					} else if key.ctrl && key.name == "u" {
						input.clear();
					} else if key.name == "backspace" {
						input.pop();
					} else if key.name == "paste" {
						input.push_str(&key.text.as_deref().unwrap_or_default().replace(['\r', '\n'], ""));
					} else if let Some(t) = key.typed() {
						input.push_str(t);
					}
					Some(Prompt::SaveAs { input, index, close })
				}
			},
			Prompt::Overwrite { path, index, close } => {
				if yes {
					if self.write_buffer(index, path.clone(), close) { None } else { Some(Prompt::SaveAs { input: path.display().to_string(), index, close }) }
				} else if no {
					Some(Prompt::SaveAs { input: path.display().to_string(), index, close })
				} else {
					Some(Prompt::Overwrite { path, index, close })
				}
			},
			Prompt::Dialog { index, close } => {
				if key.is("escape") {
					self.dialog = None;
					None
				} else {
					Some(Prompt::Dialog { index, close })
				}
			},
			Prompt::Close(i) => {
				if yes {
					match self.buffers.get(i).and_then(|b| b.path.clone()) {
						Some(p) => {
							if self.write_buffer(i, p, true) { None } else { Some(Prompt::Close(i)) }
						},
						None => {
							self.ask_where(i, true);
							self.prompt.take()
						},
					}
				} else if key.typed().is_some_and(|t| t.eq_ignore_ascii_case("n")) {
					self.close_buffer_at(i);
					None
				} else if key.is("escape") {
					None
				} else {
					Some(Prompt::Close(i))
				}
			},
		};
	}

	/// The prompt as a bar over the editor.
	fn prompt_bar(&self) -> Option<html::El<Msg>> {
		let name = |i: usize| self.buffers.get(i).map_or_else(String::new, |b| b.name.clone());
		let (label, value, hint) = match self.prompt.as_ref()? {
			Prompt::SaveAs { input, .. } => ("Save as".to_owned(), format!("{input}\u{258f}"), "enter saves, esc cancels"),
			Prompt::Overwrite { path, .. } => (
				format!("{} exists. Overwrite?", path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned())),
				String::new(),
				"y overwrites, n goes back",
			),
			Prompt::Dialog { .. } => ("Save as".to_owned(), "choose a file in the Save dialog".to_owned(), "esc cancels"),
			Prompt::Close(i) => (format!("Save changes to {}?", name(*i)), String::new(), "y saves, n discards, esc cancels"),
		};
		Some(html::div().class("prompt").child(html::span(label).class("pl")).child(html::span(value).class("pv")).child(html::span(hint).class("ph")))
	}

	/// Refuses batches that look like writes in read-only mode.
	fn allowed(&mut self, batches: &[String]) -> bool {
		if self.write {
			return true;
		}
		for b in batches {
			if let Some(word) = guard::check(b) {
				self.out = Output {
					messages: vec![format!("Read-only mode refused `{word}`. Press F8 to allow writes.")],
					error: true,
					..Output::default()
				};
				return false;
			}
		}
		true
	}

	/// Runs the whole buffer, batch by batch at `GO`.
	fn run_all(&mut self) {
		let batches = tsql::batches(&self.buffers[self.active].text);
		self.run(batches);
	}

	/// Runs the statement at the caret.
	fn run_statement(&mut self) {
		let b = &self.buffers[self.active];
		let s = tsql::statement_at(&b.text, b.cursor_byte()).to_owned();
		// Tern does not report the selection, so Ctrl+Enter after a select-all lands after the last statement:
		// running that empty text only prints "Commands completed successfully" and looks like a dead connection.
		if !tsql::has_code(&s) {
			self.popup = None;
			self.note = "nothing to run at the caret: put it inside a statement, or F5 runs the whole script".into();
			return;
		}
		self.run(vec![s]);
	}

	fn run(&mut self, batches: Vec<String>) {
		self.popup = None;
		if batches.is_empty() || !self.allowed(&batches) {
			return;
		}
		self.start(Job::Run { batches, limit: LIMIT }, "running");
	}

	/// Writes the statement at the caret to a CSV with no row cap.
	fn export(&mut self) {
		let b = &self.buffers[self.active];
		let sql = tsql::statement_at(&b.text, b.cursor_byte()).to_owned();
		if sql.is_empty() || !self.allowed(std::slice::from_ref(&sql)) {
			return;
		}
		let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
		let db: String = self.current().chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
		self.start(Job::Export { sql, path: PathBuf::from(format!("tern-mssql-{db}-{stamp}.csv")) }, "exporting");
	}

	/// Takes the workers' replies, if jobs finished. Returns whether anything changed.
	fn poll(&mut self) -> bool {
		let mut changed = false;
		for ci in 0..self.conns.len() {
			let Some(reply) = self.conns[ci].worker.poll() else {
				if let Some(p) = self.conns[ci].queue.front().filter(|p| !p.asked) {
					self.note = format!("{} {}s (esc cancels)", p.what, p.since.elapsed().as_secs());
					changed = true;
				}
				continue;
			};
			let since = self.conns[ci].queue.pop_front().map(|p| p.since);
			let took = since.map_or(0, |s| s.elapsed().as_millis());
			self.note.clear();
			match reply {
				Reply::Ran(RunResult { sets, messages, ms, error }) => {
					self.note = format!("{ms} ms");
					let tab = if error.is_some() || sets.is_empty() { sets.len() } else { 0 };
					self.out = Output { sets, messages, error: error.is_some(), tab, ..Output::default() };
					self.grid_focus = false;
				},
				Reply::Exported(Ok((path, n))) => {
					let full = std::fs::canonicalize(&path).unwrap_or(path);
					self.note = format!("{n} rows in {took} ms to {}", full.display());
				},
				Reply::Exported(Err(e)) => self.show_error(e),
				Reply::Switched(name, Ok(())) => {
					self.conns[ci].current = name;
					self.popup = None;
				},
				Reply::Switched(_, Err(e)) => self.show_error(e),
				Reply::Catalog(name, result) => {
					self.conns[ci].loading.remove(&name);
					match result {
						Ok(cat) => {
							self.conns[ci].catalogs.insert(name, cat);
						},
						Err(e) => self.show_error(e),
					}
				},
				Reply::Cancelled => self.note = "cancelled".into(),
			}
			if let Some(p) = self.conns[ci].queue.front() {
				self.note = format!("{} (esc cancels)", p.what);
			}
			changed = true;
		}
		changed
	}

	fn show_error(&mut self, e: String) {
		self.out = Output { messages: vec![e], error: true, ..Output::default() };
	}

	/// A tree row was activated.
	fn activate(&mut self, id: &str) {
		if self.role == Role::Explorer && id.starts_with("db:") {
			self.fold(id);
			return;
		}
		let Some((kind, _, tail)) = split_id(id) else { return };
		let Some(ci) = self.conn_of(id) else { return };
		match kind {
			"db:" => {
				self.ensure_catalog(ci, tail);
				if tail != self.conns[ci].current {
					self.start_on(ci, Job::Switch(tail.to_owned()), "switching");
				}
			},
			"o:" => {
				let mut parts = tail.splitn(3, SEP);
				let (Some(db), Some(schema), Some(name)) = (parts.next(), parts.next(), parts.next()) else { return };
				let Some(kind) = self.conns[ci].catalogs.get(db).and_then(|c| c.find(Some(schema), name)).map(|o| o.kind) else { return };
				// An object in another database is reached by its three-part name, so the editor's database stays put.
				let q = self.qualify(ci, db, schema, name);
				let sql = match kind {
					ObjKind::Table | ObjKind::View => format!("SELECT TOP (1000) *\nFROM {q}"),
					ObjKind::Proc => format!("EXEC {q}"),
					ObjKind::Func => format!("SELECT * FROM {q}()"),
				};
				self.open_sql(ci, db, sql, matches!(kind, ObjKind::Table | ObjKind::View));
			},
			_ => {},
		}
	}

	/// Opens a new buffer holding generated T-SQL for the selected table or view.
	fn script_selected(&mut self, kind: script::Kind) {
		let Some((ci, tail)) = self.picked.as_deref().and_then(|id| Some((self.conn_of(id)?, split_id(id).filter(|(k, ..)| *k == "o:")?.2))) else {
			self.note = "select a table or view first".into();
			return;
		};
		let mut parts = tail.splitn(3, SEP);
		let (Some(db), Some(schema), Some(name)) = (parts.next(), parts.next(), parts.next()) else { return };
		let (db, schema, name) = (db.to_owned(), schema.to_owned(), name.to_owned());
		let Some(obj) = self.conns[ci].catalogs.get(&db).and_then(|c| c.find(Some(&schema), &name)) else { return };
		if !matches!(obj.kind, ObjKind::Table | ObjKind::View) {
			self.note = "script as works on tables and views".into();
			return;
		}
		let q = self.qualify(ci, &db, &schema, &name);
		let sql = script::generate(kind, obj, &q);
		// A SELECT is safe to run on the spot; the other scripts need their placeholders filled in first.
		self.open_sql(ci, &db, sql, matches!(kind, script::Kind::Select));
	}

	/// The connection and database the selected explorer row belongs to.
	fn picked_db(&self) -> Option<(usize, String)> {
		let id = self.picked.as_deref()?;
		let ci = self.conn_of(id)?;
		let (kind, _, tail) = split_id(id)?;
		let db = match kind {
			"db:" => tail.to_owned(),
			"o:" | "c:" => tail.split(SEP).next()?.to_owned(),
			// Folders are `g:<profile>␟<db>:<group>[␟<schema>]`; neither the group nor the schema holds a colon.
			"g:" => tail.rsplit_once(':')?.0.to_owned(),
			_ => return None,
		};
		Some((ci, db))
	}

	/// Runs a database menu item on the selected row's database.
	fn db_action(&mut self, act: DbAct) {
		let Some((ci, db)) = self.picked_db() else {
			self.note = "select a database first".into();
			return;
		};
		let id = db_id(&self.conns[ci].profile, &db);
		match act {
			DbAct::Use => self.activate(&id),
			DbAct::NewQuery if self.role == Role::Explorer => self.open_sql(ci, &db, String::new(), false),
			DbAct::NewQuery => {
				self.activate(&id);
				self.new_buffer(String::new());
			},
			DbAct::Refresh => {
				self.conns[ci].catalogs.remove(&db);
				self.ensure_catalog(ci, &db);
			},
		}
	}

	/// An object's name as the editor should write it: the editor is switched to the object's database, so two parts suffice there.
	fn qualify(&self, ci: usize, db: &str, schema: &str, name: &str) -> String {
		if self.role == Role::Explorer || db == self.conns[ci].current { quoted(schema, name) } else { format!("[{}].{}", db.replace(']', "]]"), quoted(schema, name)) }
	}

	/// Opens `sql` on `db` of connection `ci`: in an editor pane from the explorer, in a new buffer otherwise.
	fn open_sql(&mut self, ci: usize, db: &str, sql: String, run: bool) {
		if self.role != Role::Explorer {
			self.new_buffer(sql.clone());
			if run {
				self.run(vec![sql]);
			}
			return;
		}
		let profile = self.conns[ci].profile.clone();
		self.note = match remote::open(&profile, &remote::Request { profile: profile.clone(), db: db.to_owned(), sql, run, quiet: false }, true) {
			Ok(()) => format!("opened in the {profile} editor ({db})"),
			Err(e) => e,
		};
	}

	/// An editor pane: opens what the explorer sent, switching to its database first.
	fn open_remote(&mut self, r: remote::Request) {
		if !r.db.is_empty() {
			self.ensure_catalog(0, &r.db);
			if r.db != self.current() {
				self.start(Job::Switch(r.db.clone()), "switching");
			}
		}
		if r.quiet && r.sql.is_empty() {
			return;
		}
		let fresh = self.buffers.len() == 1 && self.buffers[0].text.is_empty() && !self.buffers[0].dirty;
		if fresh && !r.sql.is_empty() {
			let b = &mut self.buffers[0];
			b.text = r.sql.clone();
			b.cursor = u16_len(&b.text);
		} else if !fresh {
			self.new_buffer(r.sql.clone());
		}
		self.popup = None;
		self.in_editor = true;
		self.grid_focus = false;
		if r.run && !r.sql.is_empty() {
			self.run(vec![r.sql]);
		}
	}

	/// Recomputes the completion popup after an edit or on request.
	fn complete(&mut self, force: bool) {
		let b = &self.buffers[self.active];
		let dbs = self.conns.first().map_or(&[][..], |c| c.databases.as_slice());
		let popup = complete::complete(&b.text, b.cursor_byte(), self.catalog(), dbs, force).map(|comp| Popup { comp, sel: 0 });
		self.popup = popup;
	}

	/// Inserts the selected suggestion.
	fn accept(&mut self, index: Option<usize>) {
		let Some(p) = self.popup.take() else { return };
		let Some(item) = p.comp.items.get(index.unwrap_or(p.sel)) else { return };
		let b = self.buf();
		let end = b.cursor_byte();
		b.undo.push((b.text.clone(), b.cursor));
		b.text.replace_range(p.comp.from..end, &item.insert);
		b.cursor = u16_len(&b.text[..p.comp.from + item.insert.len()]);
		b.last_single = false;
		b.dirty = true;
	}

	/// One root per connection, with its databases under it.
	fn explorer_roots(&self) -> Vec<TreeItem> {
		let open = |id: &str| self.expanded.contains(id);
		self.conns
			.iter()
			.map(|c| {
				let rid = root_id(&c.profile);
				let mut srv = TreeItem::new(rid.clone(), vec![span(c.profile.clone(), "strong"), span(format!("  {}", c.server), "dim")]).open(open(&rid));
				srv.icon = Some("server".into());
				for d in &c.databases {
					let id = db_id(&c.profile, d);
					let label = if *d == c.current && self.role != Role::Explorer { vec![span("● ", "accent"), span(d.clone(), "strong")] } else { vec![span(d.clone(), "")] };
					let mut item = TreeItem::new(id.clone(), label).open(open(&id));
					item.icon = Some("database".into());
					item.children = match c.catalogs.get(d) {
						Some(cat) => self.catalog_items(&c.profile, d, cat),
						None => {
							// A child keeps the chevron; opening the database loads its objects.
							let text = if c.loading.contains(d) { "loading objects..." } else { "..." };
							vec![TreeItem::new(format!("p:{id}"), vec![span(text, "dim")])]
						},
					};
					srv = srv.child(item);
				}
				srv
			})
			.collect()
	}

	/// The rows showing now, each with its depth.
	fn visible<'a>(item: &'a TreeItem, depth: usize, out: &mut Vec<(&'a TreeItem, usize)>) {
		out.push((item, depth));
		if item.open.unwrap_or(false) {
			for c in &item.children {
				Self::visible(c, depth + 1, out);
			}
		}
	}

	/// The tree rows showing now, over every connection.
	fn shown(&self) -> Vec<(TreeItem, usize)> {
		let roots = self.explorer_roots();
		let mut shown = Vec::new();
		for r in &roots {
			Self::visible(r, 0, &mut shown);
		}
		shown.into_iter().map(|(i, d)| (i.clone(), d)).collect()
	}

	/// The explorer: every row is an `el`, so each can carry its own right-click menu.
	fn explorer(&self) -> ui::Col<Msg> {
		let rows = self.shown().into_iter().map(|(item, depth)| {
			let id = item.id.clone();
			let has_kids = !item.children.is_empty();
			let icon = serde_json::to_value(&item.icon).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
			let glyph = match icon.as_str() {
				"server" => "⌂",
				"database" => "◉",
				"grid" => "▦",
				"eye" => "◎",
				"code" => "ƒ",
				"braces" => "λ",
				"folder" | "folder-open" => "▪",
				_ => "·",
			};
			let mut row = html::div()
				.class(format!("tr d{}{}", depth.min(5), if self.picked.as_deref() == Some(id.as_str()) { " cur" } else { "" }))
				.on_click(Msg::TreeSel(id.clone()))
				.on_dblclick(Msg::Activate(id.clone()))
				.child(html::span(if has_kids { if item.open.unwrap_or(false) { "▾" } else { "▸" } } else { " " }).class("tc").on_click(Msg::Fold(id.clone())))
				.child(html::span(glyph).class("ti"));
			for (text, style) in label_parts(&item.label) {
				row = row.child(html::span(text).class(format!("s-{}", if style.is_empty() { "plain" } else { style.as_str() })));
			}
			if id.starts_with("db:") {
				let acts = if self.role == Role::Explorer {
					vec![("New query on database", DbAct::NewQuery), ("Refresh database", DbAct::Refresh)]
				} else {
					vec![("New query on database", DbAct::NewQuery), ("Use database", DbAct::Use), ("Refresh database", DbAct::Refresh)]
				};
				for (name, act) in acts {
					row = row.on_menu(name, Msg::DbRow(id.clone(), act));
				}
			} else if id.starts_with("srv") {
				row = row.on_menu(if self.by_schema { "List objects as schema.name" } else { "Group objects by schema" }, Msg::GroupToggle);
				if self.role == Role::Explorer && let Some((_, profile, _)) = split_id(&id) {
					row = row.on_menu("Disconnect", Msg::EnvDisconnect(profile.to_owned()));
				}
			} else if id.starts_with("o:") {
				for kind in [script::Kind::Select, script::Kind::Insert, script::Kind::Update, script::Kind::Delete, script::Kind::Create] {
					row = row.on_menu(kind.act(), Msg::ScriptRow(id.clone(), kind));
				}
			}
			row
		});
		let tree = html::div().class("tree").children(rows);
		let col = ui::col()
			.key("side")
			.basis(ui::Basis::Content)
			.grow(if self.role == Role::Explorer { 1.0 } else { 0.0 })
			.shrink(1.0)
			.min(ui::Bound::h(ui::Extent::Lines(self.avail_lines())))
			.max(ui::Bound::h(ui::Extent::Lines(self.avail_lines())));
		if self.role == Role::Explorer {
			col.child(html::div().class("side").child(self.envs_box()).child(tree))
		} else {
			col.child(tree)
		}
	}

	/// The environments box: every saved connection with its state, and the buttons that act on the selected one.
	fn envs_box(&self) -> html::El<Msg> {
		let mut list = html::div().class(if self.in_envs { "envs focus" } else { "envs" }).child(
			html::div()
				.class("eh")
				.child(html::span("Environments"))
				.child(html::span(format!("{} of {} connected", self.conns.len(), self.envs.len())).class("s-dim")),
		);
		for (i, p) in self.envs.iter().enumerate() {
			let on = self.conns.iter().any(|c| c.profile == p.name);
			let busy = self.connecting.iter().any(|(n, _)| *n == p.name);
			let failed = self.env_fail.contains_key(&p.name);
			let (mark, cls) = if on { ("●", "s-accent") } else if busy { ("…", "s-dim") } else if failed { ("!", "s-bad") } else { ("○", "s-dim") };
			let state = if i == self.env_at && self.in_envs { " cur" } else if i == self.env_at { " sel" } else { "" };
			let mut row = html::div()
				.class(format!("er{state}"))
				.on_click(Msg::EnvSel(p.name.clone()))
				.on_dblclick(Msg::EnvConnect(p.name.clone()))
				.child(html::span(mark).class(format!("em {cls}")))
				.child(html::span(p.name.clone()).class("s-strong"))
				.child(html::span(if p.user.is_empty() { p.server.clone() } else { format!("{} @ {}", p.user, p.server) }).class("s-dim ed"));
			row = if on { row.on_menu("New query", Msg::EnvQuery(p.name.clone())).on_menu("Disconnect", Msg::EnvDisconnect(p.name.clone())) } else { row.on_menu("Connect", Msg::EnvConnect(p.name.clone())) };
			row = row.on_menu("Edit connection", Msg::EnvForm(Some(p.name.clone()))).on_menu("New connection", Msg::EnvForm(None));
			list = list.child(row);
		}
		let sel = self.envs.get(self.env_at).map(|p| p.name.clone());
		let on = sel.as_ref().is_some_and(|n| self.conns.iter().any(|c| c.profile == *n));
		let bt = |label: &str, msg: Msg| html::span(label.to_owned()).class("bt").on_click(msg);
		let mut bar = html::div().class("ebar");
		if let Some(n) = sel.clone() {
			if on {
				bar = bar.child(bt("New query", Msg::EnvQuery(n.clone()))).child(bt("Disconnect", Msg::EnvDisconnect(n)));
			} else {
				bar = bar.child(bt("Connect", Msg::EnvConnect(n)));
			}
		}
		bar = bar.child(bt("New", Msg::EnvForm(None)));
		if let Some(n) = sel {
			bar = bar.child(bt("Edit", Msg::EnvForm(Some(n))));
		}
		list.child(bar)
	}

	/// Reads the saved profiles for the environments box when `profiles.json` changed.
	fn load_envs(&mut self) {
		let seen = std::fs::metadata(crate::profile::path()).and_then(|m| m.modified()).ok();
		if seen.is_some() && seen == self.envs_seen {
			return;
		}
		self.envs_seen = seen;
		if let Ok(p) = crate::profile::load() {
			self.envs = p;
		}
		self.env_at = self.env_at.min(self.envs.len().saturating_sub(1));
	}

	/// Opens a connection to environment `name` on a thread; `poll_connects` takes the result. A connected one shows its tree.
	fn env_connect(&mut self, name: &str) {
		if let Some(i) = self.envs.iter().position(|p| p.name == name) {
			self.env_at = i;
		}
		if self.conns.iter().any(|c| c.profile == name) {
			self.expanded.insert(root_id(name));
			self.picked = Some(root_id(name));
			self.in_envs = false;
			return;
		}
		if self.connecting.iter().any(|(n, _)| n == name) {
			return;
		}
		let Some(p) = self.envs.iter().find(|p| p.name == name).cloned() else { return };
		self.env_fail.remove(name);
		let (tx, rx) = std::sync::mpsc::channel();
		std::thread::spawn(move || {
			let conn = (|| {
				let db = crate::backend::mssql::Mssql::connect(&p.ado()?)?;
				Conn::new(db, p.name.clone(), p.server.clone())
			})();
			let _ = tx.send(conn);
		});
		self.connecting.push((name.to_owned(), rx));
		self.note = format!("connecting to {name}...");
	}

	/// Takes connections that finished opening. True when something changed.
	fn poll_connects(&mut self) -> bool {
		let mut changed = false;
		let mut i = 0;
		while i < self.connecting.len() {
			match self.connecting[i].1.try_recv() {
				Ok(result) => {
					let (name, _) = self.connecting.remove(i);
					match result {
						Ok(conn) => {
							self.expanded.insert(root_id(&name));
							self.picked = Some(root_id(&name));
							self.note = format!("connected to {name}");
							self.conns.push(conn);
							self.remember_envs();
						},
						Err(e) => {
							self.note = format!("{name}: {e:#}");
							self.env_fail.insert(name, format!("{e:#}"));
						},
					}
					changed = true;
				},
				Err(std::sync::mpsc::TryRecvError::Empty) => i += 1,
				Err(std::sync::mpsc::TryRecvError::Disconnected) => {
					self.connecting.remove(i);
					changed = true;
				},
			}
		}
		changed
	}

	/// A new empty query on environment `name`, in that environment's editor pane (opened if need be).
	fn env_query(&mut self, name: &str) {
		let Some(ci) = self.conns.iter().position(|c| c.profile == name) else {
			self.note = format!("{name} is not connected: press Enter on it first");
			return;
		};
		let db = self.conns[ci].current.clone();
		self.open_sql(ci, &db, String::new(), false);
	}

	/// Remembers which environments are connected, for the next start in this directory.
	fn remember_envs(&self) {
		let names: Vec<String> = self.conns.iter().map(|c| c.profile.clone()).collect();
		let _ = crate::session::store_envs(&names);
	}

	/// Drops environment `name`'s connection and its rows. Its editor panes keep their own connection.
	fn env_disconnect(&mut self, name: &str) {
		let Some(i) = self.conns.iter().position(|c| c.profile == name) else { return };
		self.conns.remove(i);
		self.expanded.retain(|id| split_id(id).is_none_or(|(_, p, _)| p != name));
		if self.picked.as_deref().and_then(split_id).is_some_and(|(_, p, _)| p == name) {
			self.picked = None;
		}
		self.note = format!("disconnected {name}");
		self.remember_envs();
	}

	/// Opens the connection form in a pane below this one: blank, or on `name` to edit it.
	fn env_form(&mut self, name: Option<&str>) {
		let Ok(here) = std::env::var("TERN_PANE") else {
			self.note = "not running inside a Tern pane".into();
			return;
		};
		let Ok(exe) = std::env::current_exe() else { return };
		let mut args = vec!["split".to_owned(), here, "down".to_owned(), "--".to_owned(), exe.to_string_lossy().into_owned()];
		match name {
			Some(n) => args.extend(["--edit".to_owned(), n.to_owned()]),
			None => args.push("--new".to_owned()),
		}
		self.note = match std::process::Command::new("tern").args(&args).output() {
			Ok(o) if o.status.success() => "the form is open below; it saves to profiles.json".to_owned(),
			Ok(o) => format!("tern split failed: {}", String::from_utf8_lossy(&o.stderr).trim()),
			Err(e) => format!("tern split failed: {e}"),
		};
	}

	/// Keys while the environments box has focus. False when the key is not for it.
	fn env_key(&mut self, key: &tern_sdk::keys::Key) -> bool {
		let n = self.envs.len();
		let sel = self.envs.get(self.env_at).map(|p| p.name.clone());
		match (key.name.as_str(), key.typed()) {
			("down", _) => self.env_at = (self.env_at + 1).min(n.saturating_sub(1)),
			("up", _) => self.env_at = self.env_at.saturating_sub(1),
			("enter", _) => {
				if let Some(s) = sel {
					self.env_connect(&s);
				}
			},
			(_, Some("x")) | (_, Some("d")) => {
				if let Some(s) = sel {
					self.env_disconnect(&s);
				}
			},
			(_, Some("n")) => self.env_form(None),
			(_, Some("e")) => self.env_form(sel.as_deref()),
			(_, Some("q")) => {
				if let Some(s) = sel {
					self.env_query(&s);
				}
			},
			_ => return false,
		}
		true
	}

	/// Keys while the explorer has focus: arrows move and fold, enter opens. False when the key is not for the explorer.
	fn tree_key(&mut self, key: &tern_sdk::keys::Key) -> bool {
		let shown = self.shown();
		let at = self.picked.as_deref().and_then(|p| shown.iter().position(|(i, _)| i.id == p));
		let go = |n: usize| shown.get(n).map(|(i, _)| i.id.clone());
		match key.name.as_str() {
			"down" => self.picked = go(at.map_or(0, |a| a + 1)).or(self.picked.take()),
			"up" => self.picked = go(at.map_or(0, |a| a.saturating_sub(1))).or(self.picked.take()),
			"right" | "left" => {
				let Some((item, depth)) = at.and_then(|a| shown.get(a)) else { return true };
				let open = item.open.unwrap_or(false);
				let id = item.id.clone();
				if key.name == "right" && !open && !item.children.is_empty() {
					self.fold(&id);
				} else if key.name == "left" && open {
					self.fold(&id);
				} else if key.name == "left" {
					// To the parent: the nearest row above with a smaller depth.
					let parent = shown[..at.unwrap_or(0)].iter().rev().find(|(_, d)| d < depth).map(|(i, _)| i.id.clone());
					self.picked = parent.or(self.picked.take());
				} else if key.name == "right" {
					self.picked = go(at.map_or(0, |a| a + 1)).or(self.picked.take());
				}
			},
			"g" => self.by_schema = !self.by_schema,
			"enter" => {
				if let Some(id) = self.picked.clone() {
					self.open_row(&id);
				}
			},
			_ => return false,
		}
		true
	}

	/// Folds or unfolds a row; opening a database loads its objects.
	fn fold(&mut self, id: &str) {
		if !self.expanded.remove(id) {
			self.expanded.insert(id.to_owned());
			if let (Some(ci), Some(("db:", _, db))) = (self.conn_of(id), split_id(id)) {
				let db = db.to_owned();
				self.ensure_catalog(ci, &db);
			}
		}
	}

	/// A double click or enter on a row: folders fold, databases and objects open.
	fn open_row(&mut self, id: &str) {
		if id.starts_with("g:") || id.starts_with("srv") {
			self.fold(id);
		} else {
			self.activate(id);
		}
	}

	/// One database's folders and objects.
	fn catalog_items(&self, profile: &str, db: &str, cat: &Catalog) -> Vec<TreeItem> {
		let folder = |group: &str, title: &str, count: usize| {
			let id = format!("g:{profile}{SEP}{db}:{group}");
			let is_open = self.expanded.contains(&id);
			let mut f = TreeItem::new(id, vec![span(title.to_owned(), ""), span(format!("  {count}"), "dim")]).open(is_open);
			f.icon = Some(if is_open { "folder-open" } else { "folder" }.into());
			f
		};
		let objects = |kind: ObjKind, icon: &'static str, group: &str| -> Vec<TreeItem> {
			let mut all: Vec<(String, TreeItem)> = cat
				.of(kind)
				.map(|o| {
					let id = format!("o:{profile}{SEP}{db}{SEP}{}{SEP}{}", o.schema, o.name);
					let label = if self.by_schema { vec![span(o.name.clone(), "")] } else { vec![span(format!("{}.", o.schema), "dim"), span(o.name.clone(), "")] };
					let mut item = TreeItem::new(id.clone(), label).open(self.expanded.contains(&id));
					item.icon = Some(icon.into());
					if !o.columns.is_empty() {
						if self.expanded.contains(&id) {
							for c in &o.columns {
								let mut col = TreeItem::new(
									format!("c:{profile}{SEP}{db}{SEP}{}{SEP}{}{SEP}{}", o.schema, o.name, c.name),
									vec![
										span(c.name.clone(), if c.key { "strong" } else { "" }),
										span(format!("  {}{}", c.ty, if c.nullable { "" } else { " not null" }), "dim"),
									],
								);
								col.icon = Some(if c.key { "key" } else if c.identity { "hash" } else { "tag" }.into());
								item.children.push(col);
							}
						} else {
							item.children.push(TreeItem::new(format!("p:{id}"), vec![span("...", "dim")]));
						}
					}
					(o.schema.clone(), item)
				})
				.collect();
			if !self.by_schema {
				return all.into_iter().map(|(_, i)| i).collect();
			}
			// One folder per schema, dbo first.
			let mut schemas: Vec<String> = all.iter().map(|(sch, _)| sch.clone()).collect();
			schemas.sort_by_key(|sch| (!sch.eq_ignore_ascii_case("dbo"), sch.to_lowercase()));
			schemas.dedup();
			let mut folders = Vec::new();
			for sch in schemas {
				let (mine, rest): (Vec<_>, Vec<_>) = all.into_iter().partition(|(o, _)| *o == sch);
				all = rest;
				let mut f = folder(&format!("{group}{SEP}{sch}"), &sch, mine.len());
				f.children = mine.into_iter().map(|(_, i)| i).collect();
				folders.push(f);
			}
			folders
		};
		let mut tables = folder("tables", "Tables", cat.of(ObjKind::Table).count());
		tables.children = objects(ObjKind::Table, "grid", "tables");
		let mut views = folder("views", "Views", cat.of(ObjKind::View).count());
		views.children = objects(ObjKind::View, "eye", "views");
		let mut procs = folder("procs", "Stored Procedures", cat.of(ObjKind::Proc).count());
		procs.children = objects(ObjKind::Proc, "code", "procs");
		let mut funcs = folder("funcs", "Functions", cat.of(ObjKind::Func).count());
		funcs.children = objects(ObjKind::Func, "braces", "funcs");
		let mut prog = folder("prog", "Programmability", cat.of(ObjKind::Proc).count() + cat.of(ObjKind::Func).count());
		prog.children = vec![procs, funcs];
		vec![tables, views, prog]
	}

	/// Puts the active set's rows back as they arrived and drops the sort and filter.
	fn reset_view(&mut self) {
		let tab = self.out.tab;
		if let (Some(base), Some(set)) = (self.out.base.take(), self.out.sets.get_mut(tab)) {
			set.rows = base;
		}
		self.out.sort = None;
		self.out.filter.clear();
		self.out.typing = false;
		self.out.sel = None;
	}

	/// Rebuilds the active set's rows from the arrived rows, the filter and the sort.
	fn reshape(&mut self) {
		let tab = self.out.tab;
		let Some(set) = self.out.sets.get_mut(tab) else { return };
		let base = self.out.base.get_or_insert_with(|| set.rows.clone());
		set.rows = crate::view::shape(base, &self.out.filter, self.out.sort);
		self.out.sel = None;
		let (shown, all) = (set.rows.len(), base.len());
		let sorted = self.out.sort.map(|(c, asc)| format!("  sorted by {} {}", set.columns.get(c).map_or("", String::as_str), if asc { "asc" } else { "desc" })).unwrap_or_default();
		self.note = if self.out.filter.is_empty() && !self.out.typing {
			format!("{shown} rows{sorted}")
		} else {
			format!("filter: {}{}  ({shown} of {all} rows){sorted}", self.out.filter, if self.out.typing { "|" } else { "" })
		};
	}

	/// Sorts by a column: ascending, then descending, then as it arrived.
	fn sort_col(&mut self, c: usize) {
		self.grid_focus = true;
		self.out.sort = match self.out.sort {
			Some((o, true)) if o == c => Some((c, false)),
			Some((o, false)) if o == c => None,
			_ => Some((c, true)),
		};
		self.reshape();
	}

	/// Writes the shown rows to a JSON or INSERT file next to the CSV exports.
	fn save_view(&mut self, json: bool) {
		let Some(set) = self.out.sets.get(self.out.tab) else { return };
		let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
		let db: String = self.current().chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
		let (ext, text) = if json {
			("json", crate::view::json(&set.columns, &set.rows))
		} else {
			("sql", crate::view::inserts("[dbo].[Table]", &set.columns, &set.rows))
		};
		let path = PathBuf::from(format!("tern-mssql-{db}-{stamp}.{ext}"));
		self.note = match std::fs::write(&path, text) {
			Ok(()) => format!("wrote {} rows to {}", set.rows.len(), path.display()),
			Err(e) => format!("write failed: {e}"),
		};
	}

	/// The size of the shown result set: (rows, columns).
	fn grid_dims(&self) -> Option<(usize, usize)> {
		self.out.sets.get(self.out.tab).map(|s| (s.rows.len(), s.columns.len()))
	}

	/// A click on a cell: select it, or extend the selection to it with shift.
	fn click_cell(&mut self, r: usize, c: usize, shift: bool) {
		self.grid_focus = true;
		self.out.sel = Some(match (shift, self.out.sel) {
			(true, Some(s)) => Sel { r1: r, c1: c, ..s },
			_ => Sel { r0: r, c0: c, r1: r, c1: c },
		});
	}

	/// A click on a column header: the whole column, or a range of columns with shift.
	fn click_col(&mut self, c: usize, shift: bool) {
		let Some((rows, _)) = self.grid_dims().filter(|d| d.0 > 0) else { return };
		self.grid_focus = true;
		self.out.sel = Some(match (shift, self.out.sel) {
			(true, Some(s)) => Sel { r0: 0, r1: rows - 1, c1: c, ..s },
			_ => Sel { r0: 0, c0: c, r1: rows - 1, c1: c },
		});
	}

	/// A click on a row number: the whole row, or a range of rows with shift.
	fn click_row(&mut self, r: usize, shift: bool) {
		let Some((_, cols)) = self.grid_dims().filter(|d| d.1 > 0) else { return };
		self.grid_focus = true;
		self.out.sel = Some(match (shift, self.out.sel) {
			(true, Some(s)) => Sel { c0: 0, c1: cols - 1, r1: r, ..s },
			_ => Sel { r0: r, c0: 0, r1: r, c1: cols - 1 },
		});
	}

	/// Selects every cell.
	fn select_all(&mut self) {
		if let Some((rows, cols)) = self.grid_dims().filter(|d| d.0 > 0 && d.1 > 0) {
			self.grid_focus = true;
			self.out.sel = Some(Sel { r0: 0, c0: 0, r1: rows - 1, c1: cols - 1 });
		}
	}

	/// Keys while the grid has focus: arrows move (shift extends), ctrl+a selects all, ctrl+c copies, esc returns to the editor.
	fn grid_key(&mut self, key: &tern_sdk::keys::Key) -> bool {
		if self.out.typing {
			match key.name.as_str() {
				"escape" => {
					self.out.typing = false;
					self.out.filter.clear();
				},
				"enter" => self.out.typing = false,
				"backspace" => {
					self.out.filter.pop();
				},
				"paste" => self.out.filter.push_str(&key.text.as_deref().unwrap_or_default().replace(['\r', '\n'], " ")),
				_ => match key.typed().filter(|_| !key.ctrl && !key.meta) {
					Some(t) => self.out.filter.push_str(t),
					None => return true,
				},
			}
			self.reshape();
			return true;
		}
		if key.is("escape") {
			if !self.out.filter.is_empty() || self.out.sort.is_some() {
				self.reset_view();
				self.note = "view reset".into();
			} else {
				self.grid_focus = false;
			}
			return true;
		}
		if key.is("ctrl+f") || (key.typed() == Some("/") && !key.ctrl) {
			self.out.typing = true;
			self.reshape();
			return true;
		}
		let Some((rows, cols)) = self.grid_dims().filter(|d| d.0 > 0 && d.1 > 0) else { return false };
		if key.ctrl && key.name == "a" {
			self.select_all();
			return true;
		}
		if key.ctrl && key.name == "c" {
			self.copy(key.shift);
			return true;
		}
		if !key.ctrl && !key.meta {
			match key.typed() {
				Some("s") => {
					let c = self.out.sel.map_or(0, |s| s.c1);
					self.sort_col(c);
					return true;
				},
				Some("j") => {
					self.save_view(true);
					return true;
				},
				Some("i") => {
					self.save_view(false);
					return true;
				},
				_ => {},
			}
		}
		let mut s = self.out.sel.unwrap_or(Sel { r0: 0, c0: 0, r1: 0, c1: 0 });
		match key.name.as_str() {
			"left" => s.c1 = if key.ctrl { 0 } else { s.c1.saturating_sub(1) },
			"right" => s.c1 = if key.ctrl { cols - 1 } else { (s.c1 + 1).min(cols - 1) },
			"up" => s.r1 = if key.ctrl { 0 } else { s.r1.saturating_sub(1) },
			"down" => s.r1 = if key.ctrl { rows - 1 } else { (s.r1 + 1).min(rows - 1) },
			"home" => s.c1 = 0,
			"end" => s.c1 = cols - 1,
			"pageup" => s.r1 = s.r1.saturating_sub(10),
			"pagedown" => s.r1 = (s.r1 + 10).min(rows - 1),
			_ => return false,
		}
		if !key.shift {
			s.r0 = s.r1;
			s.c0 = s.c1;
		}
		self.out.sel = Some(s);
		true
	}

	/// The selected cells as tab-separated text, the way Excel copies them. NULL is an empty cell.
	fn tsv(&self, headers: bool) -> Option<(String, usize)> {
		let set = self.out.sets.get(self.out.tab)?;
		let (r0, r1, c0, c1) = self.out.sel?.bounds();
		let field = |t: &str| {
			if t.contains(['\t', '\n', '\r', '"']) { format!("\"{}\"", t.replace('"', "\"\"")) } else { t.to_owned() }
		};
		let mut lines: Vec<String> = Vec::new();
		if headers {
			lines.push((c0..=c1).map(|c| field(&set.columns[c])).collect::<Vec<_>>().join("\t"));
		}
		for r in r0..=r1 {
			lines.push((c0..=c1).map(|c| set.rows[r][c].as_deref().map(field).unwrap_or_default()).collect::<Vec<_>>().join("\t"));
		}
		let mut text = lines.join("\n");
		if r1 > r0 {
			text.push('\n');
		}
		Some((text, (r1 - r0 + 1) * (c1 - c0 + 1)))
	}

	/// Copies the selection to the system clipboard.
	fn copy(&mut self, headers: bool) {
		match self.tsv(headers) {
			Some((text, cells)) => match crate::clip::set(&text) {
				Ok(()) => self.note = format!("copied {cells} cell{}", if cells == 1 { "" } else { "s" }),
				Err(e) => self.note = format!("copy failed: {e}"),
			},
			None => self.note = "nothing selected".into(),
		}
	}

	/// The results area: a tab per result set, then Messages.
	fn results(&self) -> ui::Col<Msg> {
		let o = &self.out;
		if o.sets.is_empty() && o.messages.is_empty() {
			let hint = html::div().class("ln").child(html::span("Run a query with F5, or the statement at the caret with ctrl+enter. Results and messages appear here."));
			return ui::col().key("res").child(ui::col().key("body").min(ui::Bound::h(ui::Extent::Lines(self.body_lines()))).max(ui::Bound::h(ui::Extent::Lines(self.body_lines()))).child(html::div().class("box").child(hint)));
		}
		let mut tabs = ui::tabs().key("rtabs").min(ui::Bound::h(ui::Extent::Lines(2.6))).on_select(|id| Msg::ResultTab(id.to_owned()));
		for i in 0..o.sets.len() {
			tabs = tabs.tab(format!("r{i}"), format!("Results {}", i + 1));
		}
		tabs = tabs.tab("msg", if o.error { vec![span("Messages", "error")] } else { vec![span("Messages", "")] });
		tabs = tabs.active(if o.tab >= o.sets.len() { "msg".to_owned() } else { format!("r{}", o.tab) });
		let body: ui::Col<Msg> = if o.tab >= o.sets.len() {
			let mut lines = Vec::new();
			if o.messages.is_empty() {
				lines.push(html::div().class("ln").child(html::span("F5 runs the script, ctrl+enter the statement at the caret. Results and messages show here.")));
			}
			for (i, m) in o.messages.iter().enumerate() {
				let bad = o.error && i + 1 == o.messages.len();
				lines.push(html::div().class(if bad { "ln bad" } else { "ln" }).child(html::span(m.clone())));
			}
			ui::col().key("body").min(ui::Bound::h(ui::Extent::Lines(self.body_lines()))).max(ui::Bound::h(ui::Extent::Lines(self.body_lines()))).child(html::div().class("box").children(lines))
		} else {
			let r = &o.sets[o.tab];
			let class = |on: bool, base: &str| if on { format!("{base} sel") } else { base.to_owned() };
			let mut head = html::tr().child(html::th("#").class("rn").on_click(Msg::GridAll));
			for (c, name) in r.columns.iter().enumerate() {
				let on = o.sel.is_some_and(|s| s.cols().contains(&c));
				let arrow = match o.sort {
					Some((sc, true)) if sc == c => "\u{25b2}",
					Some((sc, false)) if sc == c => "\u{25bc}",
					_ => "\u{21c5}",
				};
				head = head.child(
					html::th(name.clone())
						.class(class(on, "ch"))
						.on_click(Msg::Col(c))
						.child(html::span(arrow).class(if o.sort.is_some_and(|s| s.0 == c) { "sa on" } else { "sa" }).on_click(Msg::Sort(c))),
				);
			}
			let rows = r.rows.iter().enumerate().take(RENDER_ROWS).map(|(n, row)| {
				let row_on = o.sel.is_some_and(|s| s.rows().contains(&n));
				let mut tr = html::tr().child(html::td((n + 1).to_string()).class(class(row_on, "rn")).on_click(Msg::Row(n)));
				for (c, cell) in row.iter().enumerate() {
					let picked = o.sel.is_some_and(|s| s.contains(n, c));
					let cur = o.sel.is_some_and(|s| (s.r1, s.c1) == (n, c));
					let mut cls = String::new();
					for (on, name) in [(picked, "sel"), (cur, "cur"), (cell.is_none(), "null")] {
						if on {
							cls.push_str(name);
							cls.push(' ');
						}
					}
					tr = tr.child(
						html::td(cell.as_deref().map_or_else(|| "NULL".to_owned(), flat)).class(cls.trim().to_owned()).on_click(Msg::Cell(n, c)),
					);
				}
				tr
			});
			let table = html::table().class("grid").child(html::thead().child(head)).child(html::tbody().children(rows));
			ui::col().key("body").min(ui::Bound::h(ui::Extent::Lines(self.body_lines()))).max(ui::Bound::h(ui::Extent::Lines(self.body_lines()))).child(html::div().class("gridwrap").child(table))
		};
		ui::col().key("res").child(tabs).child(body)
	}

	/// The query tabs and the active buffer's editor.
	fn work(&self) -> ui::Col<Msg> {
		let b = &self.buffers[self.active];
		// Our own strip, so every tab can carry a close button and its own menu.
		let mut tabs = html::div().class("qt");
		for (i, buf) in self.buffers.iter().enumerate() {
			let mut tab = html::div()
				.class(if i == self.active { "qtab cur" } else { "qtab" })
				.on_click(Msg::Tab(i.to_string()))
				.on_menu("Close tab", Msg::CloseTab(i))
				.on_menu("New query tab", Msg::NewTab)
				.child(html::span(self.tab_label(buf)));
			if buf.unsaved() {
				tab = tab.child(html::span("●").class("s-accent"));
			}
			tabs = tabs.child(tab.child(html::span("×").class("qx").on_click(Msg::CloseTab(i))));
		}
		tabs = tabs.child(html::div().class("qtab plus").on_click(Msg::NewTab).child(html::span("+")));
		let editor = ui::editor()
			.key(format!("ed{}", self.epoch))
			.grow(1.0)
			.grow(0.0)
			.min(ui::Bound::h(ui::Extent::Lines(self.editor_lines())))
			.max(ui::Bound::h(ui::Extent::Lines(self.editor_lines())))
			.max_lines(self.editor_lines() as u32)
			.text(b.text.clone())
			.cursor(b.cursor)
			.lang("sql")
			.decor(tsql::decorate(&b.text))
			.placeholder("-- T-SQL. F5 runs, ctrl+enter runs the statement, ctrl+space completes")
			.on_edit(|e| Msg::Edit(e.clone()))
			.on_undo(Msg::Undo)
			.on_focus(Msg::Focus);
		let mut col = ui::col().key("work").grow(1.0).shrink(1.0).min(ui::Bound::h(ui::Extent::Lines(0.0))).max(ui::Bound::h(ui::Extent::Fraction(1.0))).child(tabs);
		if let Some(bar) = self.prompt_bar() {
			col = col.child(bar);
		}
		// The popup's anchor has no height and sits at the editor's top edge; the popup is placed from there.
		if let Some(pop) = self.popup_overlay() {
			col = col.child(pop);
		}
		col.child(editor).child(html::div().class("hsep")).child(self.results())
	}


	/// The completion list's likely width in CSS pixels: its widest label and detail, plus the tag column and padding.
	fn popup_width(&self) -> Option<f64> {
		let p = self.popup.as_ref()?;
		let label = p.comp.items.iter().map(|i| i.label.chars().count()).max().unwrap_or(0);
		let detail = p.comp.items.iter().map(|i| i.detail.chars().count()).max().unwrap_or(0);
		let char_px = self.cell.0 * CSS_PER_CELL_W;
		Some(((label + detail) as f64 * char_px + 110.0).clamp(260.0, 600.0))
	}

	/// The editor's width in CSS pixels: the pane less the explorer and the divider beside it.
	fn editor_px(&self) -> f64 {
		let pane = self.cols * self.cell.0 * CSS_PER_CELL_W;
		let side = if self.role == Role::Whole { SIDES[self.side] + 24.0 } else { 0.0 };
		pane - side - 24.0
	}

	/// The completion list, shown in the dock above the status strip.
	fn popup_overlay(&self) -> Option<html::El<Msg>> {
		let p = self.popup.as_ref()?;
		let n = p.comp.items.len();
		// A window of rows that keeps the selection visible.
		let first = p.sel.saturating_sub(POPUP_ROWS - 1).min(n.saturating_sub(POPUP_ROWS));
		let rows = p.comp.items.iter().enumerate().skip(first).take(POPUP_ROWS).map(|(i, it)| {
			let tag = match it.kind {
				ItemKind::Column => "col",
				ItemKind::Table => "tbl",
				ItemKind::View => "view",
				ItemKind::Proc => "proc",
				ItemKind::Func => "fn",
				ItemKind::Schema => "sch",
				ItemKind::Database => "db",
				ItemKind::Keyword => "kw",
			};
			html::div()
				.class(if i == p.sel { "pi cur" } else { "pi" })
				.on_click(Msg::Pick(i.to_string()))
				.child(html::span(tag).class("pk"))
				.child(html::span(it.label.clone()))
				.child(html::span(it.detail.clone()).class("pd"))
		});
		// Just under the caret's line: left is the gutter plus the column, top the line count.
		Some(html::div().class("popwrap").child(html::div().class("pop").children(rows)))
	}

	/// The whole view.
	fn view(&self) -> View<Msg> {
		let b = &self.buffers[self.active];
		let (line, col) = b.line_col();
		let side = html::div().class("vsep").child(html::span("‹").class("vb").on_click(Msg::Side(-1))).child(html::span("›").class("vb").on_click(Msg::Side(1)));
		let row = ui::row().key("body").grow(1.0).shrink(1.0).min(ui::Bound::h(ui::Extent::Lines(0.0))).max(ui::Bound::h(ui::Extent::Fraction(1.0))).align(ui::Align::Stretch);
		let body = match self.role {
			Role::Explorer => row.child(self.explorer()),
			Role::Editor => row.child(self.work()),
			Role::Whole => row.child(self.explorer()).child(side).child(self.work()),
		};
		let catalog = self.catalog().map_or_else(|| "loading catalog...".to_owned(), |c| format!("{} objects", c.objects.len()));
		let head = if self.role == Role::Explorer {
			match self.conns.len() {
				0 => "no connection".to_owned(),
				_ => self.conns.iter().map(|c| c.profile.as_str()).collect::<Vec<_>>().join(", "),
			}
		} else {
			format!("{} / {}", self.profile(), self.current())
		};
		let mut status = ui::status().key("bar").child(ui::seg(head).icon("database"));
		if self.role != Role::Explorer {
			status = status
				.child(ui::seg(format!("Ln {line}, Col {col}")))
				.child(ui::seg(catalog).tone(Tone::Muted))
				.child(ui::seg(if self.native > 0 { "native edit" } else { "key edit" }).tone(Tone::Muted));
		}
		status = status.child(ui::seg(self.note.clone()).tone(Tone::Muted));
		if self.role != Role::Explorer {
			status = status.child(
				ui::seg(if self.write { "WRITE" } else { "read-only" })
					.side(Side::Right)
					.tone(if self.write { Tone::Warning } else { Tone::Success }),
			);
		}
		let main = ui::col().min(ui::Bound::h(ui::Extent::Fraction(1.0))).max(ui::Bound::h(ui::Extent::Fraction(1.0))).align(ui::Align::Stretch).child(body);
		View::new().main(main).dock(nodes![status])
	}
}

/// The results grid: cells that select and copy like a spreadsheet's.
const GRID_CSS: &str = "\
.grid{border-collapse:collapse;width:100%;font-variant-numeric:tabular-nums}\
.grid th,.grid td{border:1px solid color-mix(in srgb,var(--sf-c-muted) 40%,transparent)}\
.grid th{background:color-mix(in srgb,var(--sf-c-accent) 12%,#15171d)}\
.grid tbody tr:nth-child(even) td:not(.sel){background:color-mix(in srgb,var(--sf-c-muted) 7%,transparent)}\
.hsep{height:0;margin:3px 0}\
.qt{display:flex;align-items:center;gap:6px;height:36px;box-sizing:border-box;padding:3px 2px;font-family:Geist,system-ui,sans-serif;font-size:12.9px}\
.qtab{display:flex;align-items:center;gap:8px;height:28px;box-sizing:border-box;padding:0 8px 0 12px;border-radius:14px;cursor:pointer;color:var(--sf-c-muted);white-space:nowrap}\
.qtab.cur{color:inherit;background:color-mix(in srgb,var(--sf-c-accent) 16%,#15171d)}\
.qtab.plus{padding:0 10px;color:var(--sf-c-accent)}\
.qx{padding:0 4px;border-radius:8px;opacity:.55}\
.qx:hover{opacity:1;background:color-mix(in srgb,var(--sf-bad) 30%,transparent)}\
.side{display:flex;flex-direction:column;height:100%;gap:6px}.side .tree{flex:1 1 auto;min-height:0;height:auto}\
.envs{flex:0 0 auto;max-height:45%;overflow:auto;box-sizing:border-box;padding:4px;font-family:Geist,system-ui,sans-serif;font-size:12.9px;border:1px solid color-mix(in srgb,var(--sf-c-muted) 45%,transparent);border-radius:6px}.envs.focus{border-color:var(--sf-c-accent)}\
.eh{display:flex;justify-content:space-between;gap:8px;padding:2px 8px 4px;font-weight:600}\
.er{display:flex;align-items:center;gap:8px;height:24px;box-sizing:border-box;padding:0 8px;border-radius:4px;cursor:pointer;white-space:nowrap}.er:hover{background:color-mix(in srgb,var(--sf-c-muted) 14%,transparent)}.er.sel{background:color-mix(in srgb,var(--sf-c-muted) 20%,transparent)}.er.cur{background:color-mix(in srgb,var(--sf-c-accent) 24%,transparent)}\
.em{width:12px;flex-shrink:0;text-align:center}.ed{flex-shrink:1;min-width:0;overflow:hidden;text-overflow:ellipsis}.s-bad{color:var(--sf-bad)}\
.ebar{display:flex;flex-wrap:wrap;gap:6px;padding:4px 6px 2px}.bt{padding:1px 8px;border:1px solid color-mix(in srgb,var(--sf-c-muted) 45%,transparent);border-radius:4px;cursor:pointer;color:var(--sf-c-accent)}.bt:hover{background:color-mix(in srgb,var(--sf-c-accent) 18%,transparent)}\
.tree{height:100%;overflow:auto;box-sizing:border-box;padding:4px;font-family:Geist,system-ui,sans-serif;font-size:12.9px;border:1px solid color-mix(in srgb,var(--sf-c-muted) 45%,transparent);border-radius:6px}\
.tr{display:flex;align-items:center;gap:6px;height:24px;box-sizing:border-box;padding:0 8px;white-space:nowrap;cursor:pointer;border-radius:4px}\
.tr:hover{background:color-mix(in srgb,var(--sf-c-muted) 14%,transparent)}\
.tr.cur{background:color-mix(in srgb,var(--sf-c-accent) 24%,transparent)}\
.tc{width:12px;flex-shrink:0;color:var(--sf-c-muted);text-align:center}\
.ti{width:14px;flex-shrink:0;text-align:center;color:var(--sf-c-muted)}\
.d1{padding-left:20px}.d2{padding-left:34px}.d3{padding-left:48px}.d4{padding-left:62px}.d5{padding-left:76px}\
.tr span{flex-shrink:0}.tr .s-dim{flex-shrink:1;min-width:0;overflow:hidden;text-overflow:ellipsis}\
.s-strong{font-weight:700}.s-accent{color:var(--sf-c-accent)}.s-dim{color:var(--sf-c-muted)}\
.vsep{width:24px;align-self:stretch;display:flex;flex-direction:column;align-items:center;gap:10px;padding-top:8px}.vb{width:22px;height:30px;line-height:28px;text-align:center;border-radius:6px;border:1px solid color-mix(in srgb,var(--sf-c-muted) 45%,transparent);color:var(--sf-c-accent);cursor:pointer;font-size:22px}.vb:hover{color:var(--sf-c-accent);background:color-mix(in srgb,var(--sf-c-accent) 18%,transparent)}\
.grid th{position:sticky;top:0;text-align:left;font-weight:600;padding:2px 14px 2px 8px;white-space:nowrap;cursor:pointer;color:var(--sf-c-muted)}\
.grid td{padding:1px 14px 1px 8px;white-space:nowrap;cursor:cell}\
.grid .sa{margin-left:8px;opacity:.45;cursor:pointer}.grid .sa.on{opacity:1;color:var(--sf-c-accent)}\
.grid .rn{min-width:44px;width:44px;box-sizing:border-box;padding:1px 8px;text-align:right;overflow:visible;color:var(--sf-c-muted);cursor:pointer;user-select:none}\
.grid td.sel,.grid th.sel{background:color-mix(in srgb,var(--sf-c-accent) 40%,#15171d)}\
.grid td.cur{outline:1px solid var(--sf-c-accent);outline-offset:-1px}\
.grid td.null{font-style:italic;color:var(--sf-c-muted)}\
.gridwrap{height:100%;overflow:auto;box-sizing:border-box;width:100%;border:1px solid color-mix(in srgb,var(--sf-c-muted) 45%,transparent);border-radius:6px}.box{height:100%;box-sizing:border-box;border:1px solid color-mix(in srgb,var(--sf-c-muted) 45%,transparent);border-radius:6px;padding:6px 10px;overflow:auto;width:100%}.ln{white-space:pre-wrap;color:var(--sf-c-muted)}.ln.bad{color:var(--sf-bad)}\
.prompt{display:flex;align-items:center;gap:12px;height:30px;box-sizing:border-box;padding:0 10px;margin:2px 0;border:1px solid var(--sf-c-accent);border-radius:6px;font-family:Geist,system-ui,sans-serif;font-size:12.9px;background:color-mix(in srgb,var(--sf-c-accent) 10%,#15171d)}.prompt .pl{color:var(--sf-c-accent);font-weight:600;white-space:nowrap}.prompt .pv{flex:1;white-space:nowrap;overflow:hidden}.prompt .ph{color:var(--sf-c-muted);white-space:nowrap}\
.popwrap{position:relative;height:0;z-index:30}\
.pop{position:absolute;min-width:260px;max-width:600px;padding:2px 0;border:1px solid var(--sf-c-muted);border-radius:4px;background:color-mix(in srgb,var(--sf-c-accent) 8%,#15171d);box-shadow:0 4px 14px rgba(0,0,0,.5)}\
.pi{display:flex;gap:12px;padding:0 8px;white-space:nowrap;cursor:pointer}\
.pi.cur{background:color-mix(in srgb,var(--sf-c-accent) 30%,transparent)}\
.pk{color:var(--sf-c-accent);min-width:40px;font-size:.85em}\
.pd{color:var(--sf-c-muted);margin-left:auto;padding-left:20px}";

/// Is shift held in a click event?
fn shift_held(ev: &Event) -> bool {
	matches!(ev, Event::Action(a) if a.mods.iter().any(|m| m == "shift"))
}

/// Runs the client until the user quits.
pub fn run<B: Backend + Send + 'static>(db: B, profile: String, server: String, files: &[PathBuf], role: Role, start: Start) -> anyhow::Result<()> {
	let Some(mut session) = Session::connect(Options::new().feature(feature::SEND).feature(feature::EDIT).feature(feature::UNDO))? else {
		anyhow::bail!("interactive mode needs a terminal that speaks TSP (Tern); use `tern-mssql query` instead");
	};
	let mut app = App::new(db, profile, server, files)?;
	app.role = role;
	let title = match role {
		Role::Explorer => "SQL Explorer".to_owned(),
		Role::Editor => format!("SQL Editor · {}", app.profile().to_uppercase()),
		Role::Whole => format!("SQL Server · {}", app.profile().to_uppercase()),
	};
	if role == Role::Explorer {
		app.in_editor = false;
	}
	if role != Role::Explorer && files.is_empty() && let Some(saved) = crate::session::load(app.profile()) {
		app.restore(saved);
		app.saved_json = serde_json::to_string(&app.snapshot()).unwrap_or_default();
	}
	if role == Role::Editor {
		match remote::listen(app.profile()) {
			Ok(l) => app.remote = Some(l),
			Err(e) => app.note = format!("no link to the explorer: {e}"),
		}
		if start.db.is_some() || start.sql.is_some() {
			let db = start.db.clone().unwrap_or_default();
			app.open_remote(remote::Request { profile: app.profile().to_owned(), db, sql: start.sql.clone().unwrap_or_default(), run: start.run, quiet: start.quiet });
		}
	}
	let sf = session.open(SurfaceOptions::screen().title(title.as_str()).role("sqlq"))?;
	session.stylesheet(sf, "grid", Some(GRID_CSS))?;
	draw(&mut session, sf, &mut app)?;
	if role == Role::Explorer {
		app.ed_focused = false;
		app.load_envs();
		app.env_at = app.envs.iter().position(|p| p.name == app.profile()).unwrap_or(0);
		// The environments connected last time in this directory come back, the one just chosen being first.
		let again: Vec<String> = crate::session::load_envs().into_iter().filter(|n| n != app.profile()).collect();
		for name in &again {
			app.env_connect(name);
		}
		if !again.is_empty() {
			app.note = format!("reconnecting {} from last time", again.join(", "));
		}
		app.remember_envs();
		session.focus(sf, None)?;
		// An explorer alone is half a screen: open an editor beside it.
		// Focus stays here: the user just opened the explorer.
		let first = remote::Request { profile: app.profile().to_owned(), db: app.current().to_owned(), quiet: true, ..remote::Request::default() };
		if let Err(e) = remote::open(app.profile(), &first, false) {
			app.note = e;
		}
		draw(&mut session, sf, &mut app)?;
	} else {
		app.ed_focused = true;
		session.focus(sf, Some(app.editor_id().as_str()))?;
	}
	// TERN_MSSQL_TRACE=file logs every input, for diagnosing what Tern sends.
	let mut just_closed = false;
	let mut prompt_was = false;
	let mut autosaved = Instant::now();
	let mut rows_checked = Instant::now();
	let mut trace = std::env::var_os("TERN_MSSQL_TRACE").and_then(|p| std::fs::File::create(p).ok());
	loop {
		// Poll while jobs run so the clock ticks and replies show up; block when idle.
		let idle = app.conns.iter().all(|c| c.queue.is_empty()) && app.dialog.is_none() && app.connecting.is_empty();
		let wait = Some(Duration::from_millis(if !idle { 100 } else if app.remote.is_some() { 250 } else { 1500 }));
		let input = match session.next(wait) {
			Ok(input) => input,
			Err(tern_sdk::Error::InputClosed) => break,
			Err(e) => return Err(e.into()),
		};
		if let (Some(i), Some(log)) = (&input, &mut trace) {
			let _ = std::io::Write::write_all(log, format!("{i:?}
").as_bytes());
		}
		if app.role != Role::Explorer && autosaved.elapsed() > Duration::from_secs(1) {
			app.autosave();
			autosaved = Instant::now();
		}
		let mut changed = app.poll();
		changed |= app.poll_dialog();
		changed |= app.poll_connects();
		let requests: Vec<remote::Request> = app.remote.as_ref().map(|(_, rx)| rx.try_iter().collect()).unwrap_or_default();
		if !requests.is_empty() {
			for r in requests {
				app.open_remote(r);
			}
			app.ed_focused = true;
			session.focus(sf, Some(app.editor_id().as_str()))?;
			changed = true;
		}
		let Some(input) = input else {
			// Nothing arrived: the pane's height may have changed, and the surface is never told.
			if idle && rows_checked.elapsed() > Duration::from_millis(1500) && let Some(r) = {
				rows_checked = Instant::now();
				if app.role == Role::Explorer {
					let before = app.envs.len();
					app.load_envs();
					changed |= app.envs.len() != before;
				}
				pane_rows().filter(|r| *r != app.rows)
			} {
				app.rows = r;
				draw(&mut session, sf, &mut app)?;
				continue;
			}
			if changed {
				draw(&mut session, sf, &mut app)?;
			}
			continue;
		};
		if matches!(&input, Input::Msg(m, _) if !matches!(m, Msg::Edit(_) | Msg::Undo)) {
			app.ed_focused = false;
		}
		match input {
			Input::Key(key) => {
				let key = ctrl_from_meta(key);
				let popup = app.popup.is_some();
				if app.prompt.is_some() {
					app.prompt_key(&key);
				} else if key.is("ctrl+q") {
					break;
				} else if app.grid_focus && app.grid_key(&key) {
					if !app.grid_focus {
						app.ed_focused = true;
session.focus(sf, Some(app.editor_id().as_str()))?;
					}
				} else if popup && key.is("escape") {
					app.popup = None;
				} else if popup && (key.name == "down" || key.name == "up") {
					if let Some(p) = app.popup.as_mut() {
						let n = p.comp.items.len();
						p.sel = if key.name == "down" { (p.sel + 1) % n } else { (p.sel + n - 1) % n };
					}
				} else if popup && (key.name == "tab" || key.name == "enter") && !key.ctrl {
					app.accept(None);
				} else if key.is("f5") {
					app.run_all();
				} else if key.name == "enter" && key.ctrl {
					app.run_statement();
				} else if key.is("ctrl+space") {
					app.complete(true);
				} else if key.is("ctrl+e") {
					app.export();
				} else if key.is("escape") || key.is("ctrl+x") {
					app.cancel();
				} else if key.alt && (key.name == "left" || key.name == "right") {
					app.side = (app.side as i32 + if key.name == "right" { 1 } else { -1 }).clamp(0, SIDES.len() as i32 - 1) as usize;
				} else if key.is("f8") {
					app.write = !app.write;
				} else if key.is("ctrl+n") {
					app.new_buffer(String::new());
				} else if key.is("ctrl+w") {
					let i = app.active;
					app.request_close(i);
				} else if key.ctrl && key.shift && key.name == "s" {
					let i = app.active;
					app.ask_where(i, false);
				} else if key.is("ctrl+s") {
					app.save();
				} else if key.is("f6") && app.role == Role::Whole {
					app.in_editor = !app.in_editor;
					let id = app.editor_id();
					app.ed_focused = app.in_editor;
					session.focus(sf, app.in_editor.then_some(id.as_str()))?;
				} else if !app.grid_focus && (key.name == "paste" || key.is("ctrl+v")) {
					if app.paste(&key) {
						session.focus(sf, Some(app.editor_id().as_str()))?;
					}
				} else if key.is("f6") && app.role == Role::Explorer {
					app.in_envs = !app.in_envs;
				} else if app.role == Role::Explorer && app.in_envs && app.env_key(&key) {
				} else if !app.in_editor && !app.grid_focus {
					app.tree_key(&key);
				} else if (app.native == 0 || !app.ed_focused) && !app.grid_focus && app.buf().key(&key) {
					if !app.ed_focused {
						// Focus had left the editor; a key arriving here means it must come back.
						app.ed_focused = true;
						session.focus(sf, Some(app.editor_id().as_str()))?;
					}
					app.complete(false);
				}
			},
			Input::Msg(Msg::Edit(e), _) => {
				app.ed_focused = true;
				// Only an edit that changes the text shows Tern edits natively; a caret move from a click does not,
				// and counting it made the editor drop every typed key afterwards.
				if e.from != e.to || !e.text.is_empty() {
					app.native += 1;
				}
				if app.buf().apply(&e) {
					app.complete(false);
				} else {
					// Tern's copy of the text and ours differ; a new editor identity makes Tern take ours instead of staying stuck.
					app.epoch += 1;
					app.popup = None;
					app.note = "editor resynced".into();
					app.ed_focused = true;
session.focus(sf, Some(app.editor_id().as_str()))?;
				}
			},
			Input::Msg(Msg::Undo, _) => {
				app.buf().undo();
				app.popup = None;
			},
			Input::Msg(Msg::Focus, _) => {
				app.in_editor = true;
				app.grid_focus = false;
				app.ed_focused = true;
session.focus(sf, Some(app.editor_id().as_str()))?;
			},
			Input::Msg(Msg::Activate(id), _) => app.open_row(&id),
			Input::Msg(Msg::Fold(id), _) => app.fold(&id),
			Input::Msg(Msg::Cell(r, c), ev) => {
				app.click_cell(r, c, shift_held(&ev));
				app.ed_focused = false;
session.focus(sf, None)?;
			},
			Input::Msg(Msg::Col(c), ev) => {
				app.click_col(c, shift_held(&ev));
				app.ed_focused = false;
session.focus(sf, None)?;
			},
			Input::Msg(Msg::Row(r), ev) => {
				app.click_row(r, shift_held(&ev));
				app.ed_focused = false;
session.focus(sf, None)?;
			},
			Input::Msg(Msg::Sort(c), _) => {
				app.sort_col(c);
				app.ed_focused = false;
session.focus(sf, None)?;
			},
			Input::Msg(Msg::GridAll, _) => {
				app.select_all();
				app.ed_focused = false;
session.focus(sf, None)?;
			},
			Input::Msg(Msg::Pick(id), _) => {
				if let Ok(i) = id.parse::<usize>() {
					app.accept(Some(i));
				}
				app.ed_focused = true;
session.focus(sf, Some(app.editor_id().as_str()))?;
			},
			Input::Msg(Msg::Tab(_), _) if std::mem::take(&mut just_closed) => {},
			Input::Msg(Msg::Tab(id), _) => {
				if let Ok(i) = id.parse::<usize>() {
					if i < app.buffers.len() {
						app.active = i;
						app.popup = None;
					}
				}
				// The tab strip takes focus when clicked; typing must go back to the editor.
				app.in_editor = true;
				app.ed_focused = true;
session.focus(sf, Some(app.editor_id().as_str()))?;
			},
			Input::Msg(Msg::ResultTab(id), _) => {
				app.reset_view();
				app.out.tab = if id == "msg" { app.out.sets.len() } else { id.trim_start_matches('r').parse().unwrap_or(0) };
				app.out.sel = None;
				app.grid_focus = false;
				app.ed_focused = true;
session.focus(sf, Some(app.editor_id().as_str()))?;
			},
			Input::Msg(Msg::NewTab, _) => {
				app.new_buffer(String::new());
				app.in_editor = true;
				app.ed_focused = true;
session.focus(sf, Some(app.editor_id().as_str()))?;
			},
			Input::Msg(Msg::CloseTab(i), _) => {
				app.request_close(i);
				just_closed = true;
				app.ed_focused = true;
session.focus(sf, Some(app.editor_id().as_str()))?;
			},
			Input::Msg(Msg::GroupToggle, _) => app.by_schema = !app.by_schema,
			Input::Msg(Msg::TreeSel(id), _) => {
				app.picked = Some(id);
				app.in_envs = false;
			},
			Input::Msg(Msg::EnvSel(name), _) => {
				if let Some(i) = app.envs.iter().position(|p| p.name == name) {
					app.env_at = i;
				}
				app.in_envs = true;
			},
			Input::Msg(Msg::EnvConnect(name), _) => app.env_connect(&name),
			Input::Msg(Msg::EnvDisconnect(name), _) => app.env_disconnect(&name),
			Input::Msg(Msg::EnvForm(name), _) => app.env_form(name.as_deref()),
			Input::Msg(Msg::EnvQuery(name), _) => app.env_query(&name),
			Input::Msg(Msg::Side(d), _) => app.side = (app.side as i32 + d).clamp(0, SIDES.len() as i32 - 1) as usize,
			Input::Msg(Msg::DbRow(id, act), _) => {
				app.picked = Some(id);
				app.db_action(act);
				app.ed_focused = true;
session.focus(sf, Some(app.editor_id().as_str()))?;
			},
			Input::Msg(Msg::ScriptRow(id, kind), _) => {
				app.picked = Some(id);
				app.script_selected(kind);
				app.ed_focused = true;
session.focus(sf, Some(app.editor_id().as_str()))?;
			},
			Input::Event(Event::Resize(r)) => {
				app.rows = pane_rows().unwrap_or(app.rows);
				app.cols = f64::from(r.cols);
				if let Some(c) = r.cell {
					app.cell = (c.w, c.h);
				}
			},
			_ => continue,
		}
		// A prompt takes the keys, so the editor must not hold focus while one is up.
		if app.prompt.is_some() && app.ed_focused {
			app.ed_focused = false;
			session.focus(sf, None)?;
		} else if app.prompt.is_none() && prompt_was {
			app.in_editor = true;
			app.ed_focused = true;
			session.focus(sf, Some(app.editor_id().as_str()))?;
		}
		prompt_was = app.prompt.is_some();
		draw(&mut session, sf, &mut app)?;
	}
	if app.role != Role::Explorer {
		app.autosave();
	}
	session.close()?;
	Ok(())
}

/// Tern can report Ctrl as meta; off macOS (where meta is Cmd) treat it as Ctrl so word moves and shortcuts work.
fn ctrl_from_meta(mut key: tern_sdk::keys::Key) -> tern_sdk::keys::Key {
	if !cfg!(target_os = "macos") && key.meta && !key.ctrl {
		key.meta = false;
		key.ctrl = true;
	}
	key
}

/// Renders the view.
fn draw(session: &mut Session<Msg>, sf: tern_sdk::Surface, app: &mut App) -> anyhow::Result<()> {
	// Tern rejects inline styles and ch/lh units, so positions and heights ride in a stylesheet in pixels, resent when they change.
	let (line, col) = app.buffers[app.active].line_col();
	let (cw, ch) = app.cell;
	// Under the caret, but moved left as far as it takes for the list to stay inside the editor.
	let mut left = col.saturating_sub(1) as f64 * cw * CSS_PER_CELL_W + 12.0;
	if let Some(w) = app.popup_width() {
		left = left.min(app.editor_px() - w - 8.0).max(4.0);
	}
	let css = format!(
		".pop{{top:{}px;left:{}px}}.tree{{width:{}}}",
		line as f64 * ch * CSS_PER_CELL_H + 10.0,
		left.round(),
		if app.role == Role::Explorer { "100%".to_owned() } else { format!("{}px", SIDES[app.side]) }
	);
	if css != app.pop_css {
		session.stylesheet(sf, "pop", Some(&css))?;
		app.pop_css = css;
	}
	session.render(sf, app.view())?;
	Ok(())
}

/// The pane's height in rows, from `tern inspect --json` (the surface is never told its height).
fn pane_rows() -> Option<f64> {
	pane_dim("rows").filter(|r| *r >= 12.0)
}

/// One of the pane's dimensions (`rows` or `cols`) from `tern inspect --json`.
fn pane_dim(key: &str) -> Option<f64> {
	let id: u64 = std::env::var("TERN_PANE").ok()?.parse().ok()?;
	let out = std::process::Command::new("tern").args(["inspect", "--json"]).output().ok()?;
	let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
	let mut found = None;
	let mut stack = vec![&v];
	while let Some(n) = stack.pop() {
		match n {
			serde_json::Value::Object(m) => {
				if m.get("id").and_then(serde_json::Value::as_u64) == Some(id) && m.contains_key(key) {
					found = m.get(key).and_then(serde_json::Value::as_f64);
				}
				stack.extend(m.values());
			},
			serde_json::Value::Array(a) => stack.extend(a),
			_ => {},
		}
	}
	found
}

/// A tree label's spans as `(text, style)`.
fn label_parts(label: &impl serde::Serialize) -> Vec<(String, String)> {
	use serde_json::Value;
	let part = |v: &Value| match v {
		Value::String(t) => (t.clone(), String::new()),
		other => (
			other.get("t").and_then(Value::as_str).unwrap_or_default().to_owned(),
			other.get("s").and_then(Value::as_str).unwrap_or_default().to_owned(),
		),
	};
	match serde_json::to_value(label) {
		Ok(Value::Array(a)) => a.iter().map(part).collect(),
		Ok(v) => vec![part(&v)],
		Err(_) => Vec::new(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_click_moves_the_caret_without_editing() {
		let mut b = Buffer::new("q.sql".into(), "SELECT 1".into());
		assert!(b.apply_parts(3, 3, "", 3, 8));
		assert_eq!((b.text.as_str(), b.cursor, b.dirty, b.undo.len()), ("SELECT 1", 3, false, 0));
		assert!(b.apply_parts(8, 8, "0", 9, 8));
		assert_eq!((b.text.as_str(), b.cursor, b.dirty), ("SELECT 10", 9, true));
		// A length that disagrees means the two copies drifted.
		assert!(!b.apply_parts(0, 0, "x", 1, 3));
	}

	#[test]
	fn a_paste_is_one_undo_step_with_unix_line_endings() {
		let mut b = Buffer::new("q.sql".into(), "SELECT 1".into());
		b.cursor = 8;
		let paste = tern_sdk::keys::Key { name: "paste".into(), text: Some("\r\n-- a\r\n-- b\r".into()), ..Default::default() };
		assert!(b.key(&paste));
		assert_eq!((b.text.as_str(), b.cursor, b.undo.len()), ("SELECT 1\n-- a\n-- b\n", 19, 1));
		b.undo();
		assert_eq!((b.text.as_str(), b.cursor), ("SELECT 1", 8));
	}

	#[test]
	fn tree_ids_carry_their_connection() {
		assert_eq!(split_id(&root_id("prod")), Some(("srv", "prod", "")));
		assert_eq!(split_id(&db_id("prod", "Titan")), Some(("db:", "prod", "Titan")));
		assert_eq!(split_id(&format!("p:{}", db_id("qas", "Titan"))), Some(("db:", "qas", "Titan")));
		let obj = format!("o:qas{SEP}Titan{SEP}dbo{SEP}Orders");
		assert_eq!(split_id(&obj), Some(("o:", "qas", &format!("Titan{SEP}dbo{SEP}Orders")[..])));
		assert_eq!(split_id(&format!("g:qas{SEP}Titan:tables{SEP}dbo")).map(|(_, _, t)| t.rsplit_once(':').map(|(db, _)| db)), Some(Some("Titan")));
		assert_eq!(split_id("srv"), None);
		assert_eq!(split_id("junk"), None);
	}

	#[test]
	fn unsaved_means_edited_or_never_saved_with_text() {
		let mut b = Buffer::new("q.sql".into(), String::new());
		assert!(!b.unsaved());
		b.text = "SELECT 1".into();
		assert!(b.unsaved());
		b.path = Some(PathBuf::from("q.sql"));
		assert!(!b.unsaved());
		b.dirty = true;
		assert!(b.unsaved());
	}
}
