//! `tern-mssql [PROFILE] [FILE.sql...]`: the interactive client (a picker when PROFILE is omitted).
//! `tern-mssql query PROFILE SQL`: one batch printed as a table.
//! `tern-mssql explorer PROFILE`, `tern-mssql editor PROFILE [--db DB] [--sql-file F] [--run]`: the two panes of the split layout.
//! `tern-mssql --new`: the picker, opened on a blank new-connection form; `--edit NAME` opens it on that connection.
//! `tern-mssql profiles`: the saved profiles.

mod app;
mod backend;
mod catalog;
mod clip;
mod complete;
mod connect;
mod connstr;
mod dialog;
mod guard;
mod profile;
mod remote;
mod script;
mod session;
mod tsql;
mod view;
mod worker;

use std::path::PathBuf;

use anyhow::Context;
use backend::{Backend, Rows, mssql::Mssql};
use tern_sdk::{
	PrintOptions,
	ui::{View, html},
};

const CSS: &str = ".rs{border-spacing:0} .rs th{text-align:left;padding:0 14px 3px 0;color:var(--sf-c-muted);font-weight:500} .rs td{padding:1px 14px 1px 0} .rs .null{color:var(--sf-c-muted);font-style:italic}";

/// A result set as a table.
fn view(rows: &Rows) -> View {
	let head = html::tr().children(rows.columns.iter().map(|c| html::th(c.as_str())));
	let body = rows.rows.iter().map(|r| {
		html::tr().children(r.iter().map(|c| match c {
			Some(s) => html::td(s.replace(['\r', '\n'], " ")),
			None => html::td("NULL").class("null"),
		}))
	});
	View::new().main([html::table()
		.class("rs")
		.child(html::thead().child(head))
		.child(html::tbody().children(body))])
}

/// The named profile, connected, with its server name.
fn connect(name: &str) -> anyhow::Result<(Mssql, String)> {
	let profiles = profile::load()?;
	let p = profiles
		.iter()
		.find(|p| p.name == name)
		.with_context(|| format!("no profile named {name}"))?;
	let db = Mssql::connect(&p.ado()?).with_context(|| format!("connecting to {}", p.server))?;
	Ok((db, p.server.clone()))
}

/// What an idle pane's session can send (nothing).
#[derive(Clone, Debug)]
enum Idle {}

/// A pane that shows a hint until the explorer asks for something, then becomes the editor for that connection.
fn waiting() -> anyhow::Result<()> {
	use std::time::Duration;

	use tern_sdk::{Input, Options, Session, SurfaceOptions, ui};
	let Some(mut session) = Session::<Idle>::connect(Options::new())? else {
		anyhow::bail!("this pane needs Tern");
	};
	let sf = session.open(SurfaceOptions::screen().title("SQL Editor"))?;
	session.render(sf, View::<Idle>::new().main(ui::md("**SQL editor**

Open a table, a database or a script from the explorer, and it appears here.")))?;
	let (guard, rx) = remote::listen(remote::IDLE)?;
	let request = loop {
		if let Ok(r) = rx.recv_timeout(Duration::from_millis(200)) {
			break Some(r);
		}
		match session.next(Some(Duration::from_millis(50))) {
			Ok(Some(Input::Key(k))) if k.is("ctrl+q") => break None,
			Ok(_) => {},
			Err(tern_sdk::Error::InputClosed) => break None,
			Err(e) => return Err(e.into()),
		}
	};
	drop(guard);
	session.close()?;
	let Some(r) = request else { return Ok(()) };
	let (db, server) = connect(&r.profile)?;
	let start = app::Start { db: Some(r.db).filter(|d| !d.is_empty()), sql: Some(r.sql).filter(|s| !s.is_empty()), run: r.run, quiet: r.quiet };
	app::run(db, r.profile, server, &[], app::Role::Editor, start)
}

fn main() -> anyhow::Result<()> {
	let args: Vec<String> = std::env::args().skip(1).collect();
	// `--trace FILE` logs every input the client receives, for diagnosing what Tern sends.
	let (args, trace) = match args.iter().position(|a| a == "--trace") {
		Some(i) if i + 1 < args.len() => {
			let mut rest = args.clone();
			let file = rest.remove(i + 1);
			rest.remove(i);
			(rest, Some(file))
		},
		_ => (args, None),
	};
	if let Some(file) = trace {
		// Nothing else is running yet, so setting the environment is safe.
		unsafe { std::env::set_var("TERN_MSSQL_TRACE", file) };
	}
	match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
		["profiles"] => {
			for p in profile::load()? {
				println!("{}\t{}:{}\t{}", p.name, p.server, p.port, p.user);
			}
			Ok(())
		},
		["query", name, sql] => {
			let rows = connect(name)?.0.query(sql, 1000)?;
			tern_sdk::print(view(&rows), PrintOptions::new().css(CSS))?;
			if rows.capped {
				eprintln!("(capped at 1000 rows)");
			}
			Ok(())
		},
		[] | ["--new"] => match connect::choose(!args.is_empty(), None)? {
			Some((db, name, server)) => app::run(db, name, server, &[], app::Role::Explorer, app::Start::default()),
			None => Ok(()),
		},
		["--edit", name] => match connect::choose(false, Some(name))? {
			Some((db, name, server)) => app::run(db, name, server, &[], app::Role::Explorer, app::Start::default()),
			None => Ok(()),
		},
		["waiting"] => waiting(),
		["explorer", name] => {
			let (db, server) = connect(name)?;
			app::run(db, (*name).to_owned(), server, &[], app::Role::Explorer, app::Start::default())
		},
		["editor", name, flags @ ..] => {
			let mut start = app::Start::default();
			let mut it = flags.iter();
			while let Some(f) = it.next() {
				match *f {
					"--db" => start.db = it.next().map(|v| (*v).to_owned()).filter(|v| !v.is_empty()),
					"--run" => start.run = true,
					"--quiet" => start.quiet = true,
					"--sql-file" => {
						if let Some(path) = it.next() {
							start.sql = std::fs::read_to_string(path).ok();
							let _ = std::fs::remove_file(path);
						}
					},
					_ => {},
				}
			}
			let (db, server) = connect(name)?;
			app::run(db, (*name).to_owned(), server, &[], app::Role::Editor, start)
		},
		[name, files @ ..] => {
			let (db, server) = connect(name)?;
			let files: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
			app::run(db, (*name).to_owned(), server, &files, app::Role::Whole, app::Start::default())
		},
	}
}
