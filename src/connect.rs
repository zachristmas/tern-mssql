//! The connection manager shown when no profile is named: list, add, edit,
//! delete and test saved connections, then connect. Everything is clickable;
//! the keyboard does the same things.

use tern_sdk::{
	Input, Options, Session, SurfaceOptions,
	ui::{self, View, html},
};

use crate::{
	backend::mssql::Mssql,
	profile::{self, Profile},
};

/// What a click or key asks for.
#[derive(Clone, Debug)]
enum Pick {
	/// Select a saved connection.
	Row(usize),
	/// Connect to a saved connection.
	Connect(usize),
	New,
	Edit,
	DeleteAsk,
	DeleteYes,
	DeleteNo,
	Quit,
	/// Make a form row the one that takes typing.
	Field(usize),
	/// Fields (false) or connection string (true).
	SetString(bool),
	ToggleTrust,
	Save,
	Test,
	Cancel,
}

/// What the loop does next.
enum Flow {
	Stay,
	Connect(String),
	Quit,
}

/// Form fields, in order (the connection string is held apart).
const LABELS: [&str; 7] = ["Name", "Server", "Port", "User", "Password", "Database", "Trust certificate"];
const NAME: usize = 0;
const PORT: usize = 2;
const PASSWORD: usize = 4;
const TRUST: usize = 6;

/// A line of the form.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Row {
	Mode,
	Name,
	Server,
	Port,
	User,
	Password,
	Database,
	Trust,
	ConnString,
}

const FIELD_ROWS: [Row; 8] = [Row::Mode, Row::Name, Row::Server, Row::Port, Row::User, Row::Password, Row::Database, Row::Trust];
const STRING_ROWS: [Row; 4] = [Row::Mode, Row::Name, Row::ConnString, Row::Password];

/// A connection being added or edited.
struct Form {
	/// Field values (Trust is `yes` or `no`).
	values:   [String; 7],
	/// The connection string, when that is how it is entered.
	cs:       String,
	/// Entering a connection string instead of the separate fields.
	string:   bool,
	/// Index into the current rows.
	at:       usize,
	/// The saved name being edited, `None` for a new connection.
	original: Option<String>,
}

/// A form's result: the profile and the password that goes to the credential store.
struct Built {
	profile:  Profile,
	password: Option<String>,
}

impl Form {
	fn new(p: Option<&Profile>) -> Self {
		let Some(p) = p else {
			let mut values: [String; 7] = Default::default();
			values[PORT] = "1433".into();
			values[TRUST] = "no".into();
			return Self { values, cs: String::new(), string: false, at: 1, original: None };
		};
		let password = p.password_env.as_ref().map(|v| format!("${v}")).unwrap_or_default();
		Self {
			values:   [
				p.name.clone(),
				p.server.clone(),
				p.port.to_string(),
				p.user.clone(),
				password,
				p.database.clone().unwrap_or_default(),
				if p.trust_cert { "yes" } else { "no" }.into(),
			],
			cs:       p.connection_string.clone().unwrap_or_default(),
			string:   p.connection_string.is_some(),
			at:       1,
			original: Some(p.name.clone()),
		}
	}

	fn rows(&self) -> &'static [Row] {
		if self.string { &STRING_ROWS } else { &FIELD_ROWS }
	}

	fn row(&self) -> Row {
		let rows = self.rows();
		rows[self.at.min(rows.len() - 1)]
	}

	/// The text a row edits, if it edits text.
	fn slot(&mut self, r: Row) -> Option<&mut String> {
		match r {
			Row::Name => Some(&mut self.values[NAME]),
			Row::Server => Some(&mut self.values[1]),
			Row::Port => Some(&mut self.values[PORT]),
			Row::User => Some(&mut self.values[3]),
			Row::Password => Some(&mut self.values[PASSWORD]),
			Row::Database => Some(&mut self.values[5]),
			Row::ConnString => Some(&mut self.cs),
			Row::Mode | Row::Trust => None,
		}
	}

	/// Switches between the fields and the connection string, carrying what it can across.
	fn switch(&mut self) -> Result<(), String> {
		if self.string {
			let sum = crate::connstr::summarize(&self.cs)?;
			let (_, pw) = crate::connstr::strip_password(&self.cs);
			self.values[1] = sum.server;
			self.values[PORT] = sum.port.to_string();
			self.values[3] = sum.user;
			self.values[5] = sum.database.unwrap_or_default();
			self.values[TRUST] = if sum.trust { "yes" } else { "no" }.into();
			if let Some(pw) = pw.filter(|_| self.values[PASSWORD].is_empty()) {
				self.values[PASSWORD] = pw;
			}
			self.string = false;
		} else {
			let v = &self.values;
			let mut cs = format!("Server=tcp:{},{};", v[1].trim(), v[PORT].trim());
			if !v[3].trim().is_empty() {
				cs.push_str(&format!("User Id={};", v[3].trim()));
			}
			if !v[5].trim().is_empty() {
				cs.push_str(&format!("Database={};", v[5].trim()));
			}
			cs.push_str("Encrypt=true;");
			if v[TRUST] == "yes" {
				cs.push_str("TrustServerCertificate=true;");
			}
			self.cs = cs;
			self.string = true;
		}
		self.at = 0;
		Ok(())
	}

	/// The profile these values describe, and the password to store.
	fn built(&self) -> Result<Built, String> {
		let v = &self.values;
		let name = v[NAME].trim();
		if name.is_empty() || name.contains(['\\', '/', '\n']) {
			return Err("Name is required".into());
		}
		let pw = v[PASSWORD].trim();
		let password_env = pw.strip_prefix('$').map(str::to_owned);
		let typed = self.literal().map(str::to_owned);
		if self.string {
			let sum = crate::connstr::summarize(&self.cs)?;
			let (stripped, in_string) = crate::connstr::strip_password(&self.cs);
			return Ok(Built {
				profile:  Profile {
					name: name.to_owned(),
					server: sum.server,
					port: sum.port,
					user: sum.user,
					password_env,
					database: sum.database,
					trust_cert: sum.trust,
					connection_string: Some(stripped),
				},
				password: typed.or(in_string),
			});
		}
		if v[1].trim().is_empty() {
			return Err("Server is required".into());
		}
		let port = v[PORT].trim().parse::<u16>().map_err(|_| "Port must be a number".to_owned())?;
		Ok(Built {
			profile:  Profile {
				name: name.to_owned(),
				server: v[1].trim().to_owned(),
				port,
				user: v[3].trim().to_owned(),
				password_env,
				database: Some(v[5].trim().to_owned()).filter(|d| !d.is_empty()),
				trust_cert: v[TRUST] == "yes",
				connection_string: None,
			},
			password: typed,
		})
	}

	/// The password typed, if it is a literal one.
	fn literal(&self) -> Option<&str> {
		let pw = self.values[PASSWORD].as_str();
		(!pw.is_empty() && !pw.starts_with('$')).then_some(pw)
	}

	/// Applies a key to the active row. Returns a message to show, if any.
	fn key(&mut self, k: &tern_sdk::keys::Key) -> Option<String> {
		let last = self.rows().len() - 1;
		match k.name.as_str() {
			"down" | "tab" => {
				self.at = (self.at + 1).min(last);
				return None;
			},
			"up" => {
				self.at = self.at.saturating_sub(1);
				return None;
			},
			_ => {},
		}
		let row = self.row();
		if row == Row::Mode {
			if matches!(k.name.as_str(), "left" | "right") || k.typed() == Some(" ") {
				return self.switch().err();
			}
			return None;
		}
		if row == Row::Trust {
			if let Some(s) = k.typed().filter(|s| matches!(*s, " " | "y" | "n")) {
				self.values[TRUST] = if s == "n" || self.values[TRUST] == "yes" { "no" } else { "yes" }.into();
			}
			return None;
		}
		match k.name.as_str() {
			"backspace" => {
				if let Some(v) = self.slot(row) {
					v.pop();
				}
			},
			"paste" => {
				let text = k.text.as_deref().unwrap_or_default().replace(['\r', '\n'], "");
				if let Some(v) = self.slot(row) {
					v.push_str(&text);
				}
			},
			_ => {
				if let Some(s) = k.typed().filter(|_| !k.ctrl && !k.meta).filter(|s| !s.contains(['\r', '\n'])) {
					let s = s.to_owned();
					if let Some(v) = self.slot(row) {
						v.push_str(&s);
					}
				}
			},
		}
		None
	}
}

/// A connection string with its password hidden, for showing.
fn masked(cs: &str) -> String {
	crate::connstr::parse(cs)
		.iter()
		.map(|p| if matches!(p.key.as_str(), "password" | "pwd") { "Password=******".to_owned() } else { p.raw.clone() })
		.collect::<Vec<_>>()
		.join(";")
}

/// Which screen is up.
enum Mode {
	List,
	Form(Form),
	/// Waiting for a yes to delete the selected connection.
	Delete,
}

struct State {
	profiles: Vec<Profile>,
	at:       usize,
	mode:     Mode,
	note:     String,
	/// The note reports a problem.
	bad:      bool,
}

const CSS: &str = "\
.cm{box-sizing:border-box;width:100%;max-width:100%;overflow-x:hidden;padding:8px 10px;font-family:Geist,system-ui,sans-serif;font-size:12.9px}\
.cm *{min-width:0;box-sizing:border-box}\
.ct{font-weight:700;font-size:14px;margin:0 0 8px}\
.cl{border:1px solid color-mix(in srgb,var(--sf-c-muted) 45%,transparent);border-radius:6px;padding:3px}\
.cr{display:block;padding:5px 10px;border-radius:5px;border:1px solid transparent;cursor:pointer}\
.cr:hover{background:color-mix(in srgb,var(--sf-c-muted) 14%,transparent)}\
.cr.sel{background:color-mix(in srgb,var(--sf-c-accent) 24%,transparent);border-color:var(--sf-c-accent)}\
.cn{display:block;font-weight:600}\
.cd{display:block;color:var(--sf-c-muted);overflow:hidden;text-overflow:ellipsis;white-space:nowrap}\
.bar{display:flex;flex-wrap:wrap;align-items:center;gap:6px;margin-top:10px}\
.bt{display:inline-flex;align-items:center;height:26px;padding:0 12px;border-radius:6px;border:1px solid var(--sf-c-accent);color:var(--sf-c-accent);cursor:pointer;white-space:nowrap}\
.bt:hover{background:color-mix(in srgb,var(--sf-c-accent) 20%,transparent)}\
.bt.pri{background:color-mix(in srgb,var(--sf-c-accent) 32%,transparent);color:inherit;font-weight:600}\
.bt.bad{border-color:var(--sf-bad);color:var(--sf-bad)}\
.ask{width:100%;margin-bottom:2px}\
.fr{display:block;padding:4px 8px 6px;margin:2px 0;border-radius:6px;border:1px solid transparent;cursor:pointer}\
.fr:hover{background:color-mix(in srgb,var(--sf-c-muted) 10%,transparent)}\
.fr.on{border-color:var(--sf-c-accent);background:color-mix(in srgb,var(--sf-c-accent) 12%,transparent)}\
.fl{display:block;margin-bottom:3px;color:var(--sf-c-muted);font-size:12px}.fr.on .fl{color:var(--sf-c-accent);font-weight:600}\
.fv{display:block;width:100%;min-height:24px;padding:3px 8px;border-radius:5px;border:1px solid color-mix(in srgb,var(--sf-c-muted) 45%,transparent);overflow:hidden;text-overflow:ellipsis;white-space:nowrap}\
.fv.wrap{white-space:normal;word-break:break-all}\
.fr.on .fv{border-color:var(--sf-c-accent);background:color-mix(in srgb,#000 30%,transparent)}\
.cu{color:var(--sf-c-accent)}\
.seg{display:flex;width:100%;border:1px solid var(--sf-c-accent);border-radius:6px;overflow:hidden}\
.sg{flex:1;padding:4px 6px;text-align:center;cursor:pointer;color:var(--sf-c-muted)}.sg:hover{color:inherit}\
.sg.on{background:color-mix(in srgb,var(--sf-c-accent) 32%,transparent);color:inherit;font-weight:600}\
.ck{display:flex;align-items:center;gap:8px;cursor:pointer}.ck .bx{color:var(--sf-c-accent);font-size:15px}\
.nt{margin-top:8px;color:var(--sf-c-muted);word-break:break-word}.nt.bad{color:var(--sf-bad)}\
.hp{margin-top:10px;color:var(--sf-c-muted);font-size:12px;word-break:break-word}";

fn button(label: &str, class: &str, msg: Pick) -> html::El<Pick> {
	html::span(label.to_owned()).class(format!("bt {class}")).on_click(msg)
}

/// The saved connections or the form.
fn view(s: &State) -> View<Pick> {
	let mut page = html::div().class("cm");
	let title = match &s.mode {
		Mode::Form(f) if f.original.is_some() => "Edit connection",
		Mode::Form(_) => "New connection",
		_ => "Connect to",
	};
	page = page.child(html::div().class("ct").child(html::span(title)));
	match &s.mode {
		Mode::Form(f) => {
			for (i, row) in f.rows().iter().enumerate() {
				let on = i == f.at;
				let mut line = html::div().class(if on { "fr on" } else { "fr" }).on_click(Pick::Field(i));
				let label = match row {
					Row::Mode => "Enter as",
					Row::ConnString => "Connection string",
					Row::Password => "Password",
					Row::Name => LABELS[NAME],
					Row::Server => LABELS[1],
					Row::Port => LABELS[PORT],
					Row::User => LABELS[3],
					Row::Database => LABELS[5],
					Row::Trust => "Certificate",
				};
				line = line.child(html::span(label).class("fl"));
				line = match row {
					Row::Mode => line.child(
						html::div()
							.class("seg")
							.child(html::span("Fields").class(if f.string { "sg" } else { "sg on" }).on_click(Pick::SetString(false)))
							.child(html::span("String").class(if f.string { "sg on" } else { "sg" }).on_click(Pick::SetString(true))),
					),
					Row::Trust => line.child(
						html::div()
							.class("ck")
							.on_click(Pick::ToggleTrust)
							.child(html::span(if f.values[TRUST] == "yes" { "\u{2611}" } else { "\u{2610}" }).class("bx"))
							.child(html::span("Trust server certificate")),
					),
					_ => {
						let mut shown = match row {
							Row::ConnString => masked(&f.cs),
							Row::Password if f.literal().is_some() => "*".repeat(f.values[PASSWORD].chars().count()),
							Row::Password => f.values[PASSWORD].clone(),
							Row::Name => f.values[NAME].clone(),
							Row::Server => f.values[1].clone(),
							Row::Port => f.values[PORT].clone(),
							Row::User => f.values[3].clone(),
							_ => f.values[5].clone(),
						};
						if shown.is_empty() && !on {
							shown = match row {
								Row::Password => "stored in the OS credential store, or $ENVVAR".to_owned(),
								Row::Database => "optional".to_owned(),
								_ => String::new(),
							};
						}
						let mut value = html::div().class(if *row == Row::ConnString { "fv wrap" } else { "fv" }).child(html::span(shown));
						if on {
							value = value.child(html::span("\u{258f}").class("cu"));
						}
						line.child(value)
					},
				};
				page = page.child(line);
			}
			page = page.child(
				html::div()
					.class("bar")
					.child(button("Save", "pri", Pick::Save))
					.child(button("Test connection", "", Pick::Test))
					.child(button("Cancel", "", Pick::Cancel)),
			);
			page = page.child(html::div().class("hp").child(html::span("Click a field, then type. Enter saves, Esc cancels.")));
		},
		_ => {
			if s.profiles.is_empty() {
				page = page.child(html::div().class("nt").child(html::span("No connections yet.")));
				page = page.child(html::div().class("bar").child(button("New connection", "pri", Pick::New)).child(button("Quit", "", Pick::Quit)));
			} else {
				let mut list = html::div().class("cl");
				for (i, p) in s.profiles.iter().enumerate() {
					list = list.child(
						html::div()
							.class(if i == s.at { "cr sel" } else { "cr" })
							.on_click(Pick::Row(i))
							.on_dblclick(Pick::Connect(i))
							.child(html::span(p.name.clone()).class("cn"))
							.child(html::span(if p.user.is_empty() { p.server.clone() } else { format!("{} @ {}", p.user, p.server) }).class("cd")),
					);
				}
				page = page.child(list);
				let name = s.profiles.get(s.at).map_or("", |p| p.name.as_str());
				let bar = if matches!(s.mode, Mode::Delete) {
					html::div()
						.class("bar")
						.child(html::div().class("ask").child(html::span(format!("Delete {name}?"))))
						.child(button("Delete", "bad", Pick::DeleteYes))
						.child(button("Keep it", "", Pick::DeleteNo))
				} else {
					html::div()
						.class("bar")
						.child(button("Connect", "pri", Pick::Connect(s.at)))
						.child(button("New", "", Pick::New))
						.child(button("Edit", "", Pick::Edit))
						.child(button("Delete", "", Pick::DeleteAsk))
						.child(button("Quit", "", Pick::Quit))
				};
				page = page.child(bar);
				page = page.child(html::div().class("hp").child(html::span("Click to select, double-click to connect.")));
			}
		},
	}
	if !s.note.is_empty() {
		page = page.child(html::div().class(if s.bad { "nt bad" } else { "nt" }).child(html::span(s.note.clone())));
	}
	View::new().main(ui::col().key("cm").child(page))
}

/// Tries a connection and reports the outcome as display text.
fn test(p: &Profile, password: Option<&str>) -> Result<Mssql, String> {
	let ado = match password {
		Some(pw) => p.ado_with(pw),
		None => p.ado(),
	}
	.map_err(|e| format!("{e:#}"))?;
	Mssql::connect(&ado).map_err(|e| format!("{e:#}"))
}

/// Saves the form: password to the credential store, the rest to the profile file.
fn save(s: &mut State, f: &Form) -> Result<String, String> {
	let Built { profile: p, password } = f.built()?;
	if s.profiles.iter().any(|o| o.name == p.name && f.original.as_deref() != Some(&o.name)) {
		return Err(format!("A connection named {} already exists", p.name));
	}
	if let Some(pw) = password.as_deref() {
		profile::set_secret(&p.name, pw).map_err(|e| format!("{e:#}"))?;
	} else if p.password_env.is_some() {
		profile::forget_secret(&p.name);
	} else if f.original.as_deref() != Some(&p.name) {
		if let Some(old) = &f.original {
			// A rename keeps the stored password by moving it.
			if let Ok(e) = keyring::Entry::new("tern-mssql", old)
				&& let Ok(pw) = e.get_password()
			{
				profile::set_secret(&p.name, &pw).map_err(|e| format!("{e:#}"))?;
				profile::forget_secret(old);
			}
		}
	}
	match &f.original {
		Some(old) => {
			if let Some(slot) = s.profiles.iter_mut().find(|o| &o.name == old) {
				*slot = p.clone();
			}
		},
		None => s.profiles.push(p.clone()),
	}
	profile::store(&s.profiles).map_err(|e| format!("{e:#}"))?;
	s.at = s.profiles.iter().position(|o| o.name == p.name).unwrap_or(0);
	Ok(format!("Saved {}", p.name))
}

/// Applies one request. The form is taken out of the state while it is saved or tested.
fn handle(s: &mut State, p: Pick) -> Flow {
	match p {
		Pick::Row(i) => {
			if !s.profiles.is_empty() && !matches!(s.mode, Mode::Form(_)) {
				s.at = i.min(s.profiles.len() - 1);
				s.mode = Mode::List;
				s.note.clear();
			}
		},
		Pick::Connect(i) => {
			if let Some(p) = s.profiles.get(i).filter(|_| !matches!(s.mode, Mode::Form(_))) {
				let name = p.name.clone();
				s.at = i;
				return Flow::Connect(name);
			}
		},
		Pick::New => {
			if !matches!(s.mode, Mode::Form(_)) {
				s.mode = Mode::Form(Form::new(None));
				s.note.clear();
			}
		},
		Pick::Edit => {
			if let Some(p) = s.profiles.get(s.at).filter(|_| !matches!(s.mode, Mode::Form(_))) {
				s.mode = Mode::Form(Form::new(Some(p)));
				s.note.clear();
			}
		},
		Pick::DeleteAsk => {
			if !s.profiles.is_empty() && matches!(s.mode, Mode::List) {
				s.mode = Mode::Delete;
				s.note.clear();
			}
		},
		Pick::DeleteYes => {
			if matches!(s.mode, Mode::Delete) && s.at < s.profiles.len() {
				let gone = s.profiles.remove(s.at);
				profile::forget_secret(&gone.name);
				match profile::store(&s.profiles) {
					Ok(()) => {
						s.note = format!("Deleted {}", gone.name);
						s.bad = false;
					},
					Err(e) => {
						s.note = format!("{e:#}");
						s.bad = true;
					},
				}
				s.at = s.at.min(s.profiles.len().saturating_sub(1));
			}
			s.mode = Mode::List;
		},
		Pick::DeleteNo => s.mode = Mode::List,
		Pick::Quit => return Flow::Quit,
		Pick::Field(i) => {
			if let Mode::Form(f) = &mut s.mode {
				f.at = i.min(f.rows().len() - 1);
			}
		},
		Pick::SetString(want) => {
			let err = match &mut s.mode {
				Mode::Form(f) if f.string != want => f.switch().err(),
				_ => None,
			};
			if let Some(e) = err {
				s.note = e;
				s.bad = true;
			}
		},
		Pick::ToggleTrust => {
			if let Mode::Form(f) = &mut s.mode {
				f.values[TRUST] = if f.values[TRUST] == "yes" { "no" } else { "yes" }.into();
			}
		},
		Pick::Save => {
			if let Mode::Form(f) = std::mem::replace(&mut s.mode, Mode::List) {
				match save(s, &f) {
					Ok(note) => {
						s.note = note;
						s.bad = false;
					},
					Err(e) => {
						s.note = e;
						s.bad = true;
						s.mode = Mode::Form(f);
					},
				}
			}
		},
		Pick::Test => {
			if let Mode::Form(f) = &s.mode {
				match f.built().and_then(|b| test(&b.profile, b.password.as_deref())) {
					Ok(_) => {
						s.note = "Connection works".into();
						s.bad = false;
					},
					Err(e) => {
						s.note = e;
						s.bad = true;
					},
				}
			}
		},
		Pick::Cancel => {
			s.mode = Mode::List;
			s.note.clear();
			if s.profiles.is_empty() {
				return Flow::Quit;
			}
		},
	}
	Flow::Stay
}

/// Shows the connections and returns a live connection with its profile name and server, or `None` on quit.
pub fn choose(new: bool, edit: Option<&str>) -> anyhow::Result<Option<(Mssql, String, String)>> {
	let Some(mut session) = Session::<Pick>::connect(Options::new())? else {
		anyhow::bail!("pick a profile by name outside Tern: tern-mssql PROFILE");
	};
	let sf = session.open(SurfaceOptions::screen().title("SQL Server"))?;
	session.stylesheet(sf, "cm", Some(CSS))?;
	let profiles = profile::load()?;
	let at = edit.and_then(|n| profiles.iter().position(|p| p.name == n)).unwrap_or(0);
	let mode = if edit.is_some() && at < profiles.len() {
		Mode::Form(Form::new(Some(&profiles[at])))
	} else if new || profiles.is_empty() {
		Mode::Form(Form::new(None))
	} else {
		Mode::List
	};
	let mut s = State { profiles, at, mode, note: String::new(), bad: false };
	session.render(sf, view(&s))?;
	session.focus(sf, None)?;
	let chosen = loop {
		let input = match session.next(None) {
			Ok(Some(i)) => i,
			Ok(None) => continue,
			Err(tern_sdk::Error::InputClosed) => break None,
			Err(e) => return Err(e.into()),
		};
		let pick = match input {
			Input::Msg(p, _) => Some(p),
			Input::Key(k) => key_pick(&mut s, &k),
			_ => continue,
		};
		if let Some(p) = pick {
			match handle(&mut s, p) {
				Flow::Stay => {},
				Flow::Quit => break None,
				Flow::Connect(name) => {
					s.note = format!("Connecting to {name}...");
					s.bad = false;
					session.render(sf, view(&s))?;
					let p = s.profiles[s.at].clone();
					match test(&p, None) {
						Ok(db) => break Some((db, p.name, p.server)),
						Err(e) => {
							s.note = e;
							s.bad = true;
						},
					}
				},
			}
		}
		session.render(sf, view(&s))?;
		session.focus(sf, None)?;
	};
	session.close()?;
	Ok(chosen)
}

/// A key as a request. Typing in the form edits it directly and asks for nothing.
fn key_pick(s: &mut State, k: &tern_sdk::keys::Key) -> Option<Pick> {
	match &mut s.mode {
		Mode::Form(f) => {
			if k.is("escape") {
				Some(Pick::Cancel)
			} else if k.is("enter") {
				Some(Pick::Save)
			} else if k.is("ctrl+t") {
				Some(Pick::Test)
			} else {
				let msg = f.key(k);
				s.bad = msg.is_some();
				s.note = msg.unwrap_or_default();
				None
			}
		},
		Mode::Delete => Some(if k.typed().is_some_and(|t| t.eq_ignore_ascii_case("y")) { Pick::DeleteYes } else { Pick::DeleteNo }),
		Mode::List => {
			s.note.clear();
			if k.is("escape") || k.is("ctrl+q") || k.is("ctrl+c") {
				return Some(Pick::Quit);
			}
			match (k.name.as_str(), k.typed()) {
				("enter", _) => Some(Pick::Connect(s.at)),
				("down", _) => Some(Pick::Row((s.at + 1).min(s.profiles.len().saturating_sub(1)))),
				("up", _) => Some(Pick::Row(s.at.saturating_sub(1))),
				(_, Some("n")) => Some(Pick::New),
				(_, Some("e")) => Some(Pick::Edit),
				(_, Some("d")) => Some(Pick::DeleteAsk),
				_ => None,
			}
		},
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_pasted_string_keeps_its_password_out_of_the_profile() {
		let mut f = Form::new(None);
		f.values[NAME] = "qas".into();
		f.cs = "Server=tcp:qas-db.example.com,1450;User Id=sa;Password=secret;Database=AppDb;TrustServerCertificate=true".into();
		f.string = true;
		let b = f.built().unwrap();
		assert_eq!(b.password.as_deref(), Some("secret"));
		let p = b.profile;
		assert_eq!((p.server.as_str(), p.port, p.user.as_str(), p.trust_cert), ("qas-db.example.com", 1450, "sa", true));
		assert!(!p.connection_string.as_deref().unwrap().to_lowercase().contains("password"));
		assert_eq!(masked(&f.cs).matches("secret").count(), 0);
	}

	#[test]
	fn switching_modes_carries_the_values() {
		let mut f = Form::new(None);
		f.values[NAME] = "qas".into();
		f.values[1] = "h.example.com".into();
		f.values[3] = "sa".into();
		f.values[5] = "AppDb".into();
		f.values[TRUST] = "yes".into();
		f.switch().unwrap();
		assert!(f.string && f.cs.contains("Server=tcp:h.example.com,1433") && f.cs.contains("Database=AppDb") && f.cs.contains("TrustServerCertificate=true"));
		f.cs = "Server=other,1500;User Id=bob".into();
		f.switch().unwrap();
		assert_eq!((f.values[1].as_str(), f.values[PORT].as_str(), f.values[3].as_str(), f.string), ("other", "1500", "bob", false));
		assert!(Form { cs: "Database=x".into(), string: true, ..Form::new(None) }.switch().is_err());
	}
}
