//! Explicit CLI lifecycle and a command-completion REPL. Picking/confirming
//! commands never starts a daemon or writes a file until the final approval.

use std::{
    env, fs,
    io::{self, Write},
    path::{Component, Path, PathBuf},
};

use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyEventKind},
    style::Style,
    text::Line,
    widgets::{Block, List, ListItem, ListState},
};

use crate::{
    app, client,
    global::{self, Catalog},
    ipc::Request,
    session,
    state::Session,
};

pub const USAGE: &str = "\
nui-yalij: shared sessions, independent decorations

usage: nui-yalij [--state <global.kdl>] [--config <dir>] [--profile <name>]
                 [--repl] [command] [--apply]

commands:
  attach [<session>]  connect; create a missing session (the default command)
                     inside a pane, switch the instance that last typed there
  list               list sessions and connected instances, without starting them
  reload             reload global/profile decoration and plugin files
  kill [--apply]     observe, or stop this catalog's servers and all its clients
  server             host the session servers in the foreground
  install            placeholder until a more stable release
  update             placeholder until a more stable release
  remove             placeholder until a more stable release
  help               show this help

--repl completes a command interactively (arrows or j/k, enter, esc), then
asks Apply? (N/y). Nothing executes before y, even with --apply.

State: $XDG_STATE_HOME/nui-yalij/global-state.kdl (~/.local/state/...).
Decoration: $XDG_CONFIG_HOME/nui-yalij (~/.config/...). Profiles are per
instance; active tabs/panes are shared state. ctrl-g opens navigation:
q disconnects only this instance; Q stops this catalog's servers.
";

const COMMANDS: [&str; 9] = [
    "attach", "list", "reload", "kill", "server", "install", "update", "remove", "help",
];

pub struct Options {
    pub state: PathBuf,
    pub config: PathBuf,
    pub profile: Option<String>,
    pub words: Vec<String>,
    pub repl: bool,
    pub apply: bool,
}

fn home() -> PathBuf {
    env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

pub fn default_path() -> PathBuf {
    if let Some(path) = env::var_os("NUI_YALIJ_STATE") {
        return path.into();
    }
    env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/state"))
        .join("nui-yalij/global-state.kdl")
}

fn default_config() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
        .join("nui-yalij")
}

/// Resolve aliases even when the last directories/file do not exist yet. The
/// same file must always select the same socket and lock.
pub fn absolute(path: &Path) -> Result<PathBuf, String> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .map_err(|error| error.to_string())?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            part => normalized.push(part.as_os_str()),
        }
    }
    let mut ancestor = normalized.clone();
    let mut tail = Vec::new();
    while !ancestor.exists() {
        tail.push(
            ancestor
                .file_name()
                .ok_or("path has no existing ancestor")?
                .to_os_string(),
        );
        if !ancestor.pop() {
            return Err("path has no existing ancestor".into());
        }
    }
    let mut resolved = ancestor.canonicalize().map_err(|error| error.to_string())?;
    for part in tail.iter().rev() {
        resolved.push(part);
    }
    Ok(resolved)
}

impl Options {
    pub fn parse(args: Vec<String>) -> Result<Options, String> {
        let mut options = Options {
            state: default_path(),
            config: default_config(),
            profile: None,
            words: Vec::new(),
            repl: false,
            apply: false,
        };
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let (flag, inline) = arg
                .split_once('=')
                .map_or((arg.as_str(), None), |(flag, value)| {
                    (flag, Some(value.to_string()))
                });
            match flag {
                "--state" | "--config" | "--profile" => {
                    let value = inline
                        .or_else(|| args.next())
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| format!("{flag} needs a value"))?;
                    match flag {
                        "--state" => options.state = value.into(),
                        "--config" => options.config = value.into(),
                        _ => options.profile = Some(value),
                    }
                }
                "--repl" if inline.is_none() => options.repl = true,
                "--apply" if inline.is_none() => options.apply = true,
                "-h" | "--help" => {
                    options.words = vec!["help".into()];
                    return Ok(options);
                }
                _ if arg.starts_with('-') => return Err(format!("unknown option {arg}")),
                _ => options.words.push(arg),
            }
        }
        options.state = absolute(&options.state)?;
        options.config = absolute(&options.config)?;
        if options
            .profile
            .as_ref()
            .is_some_and(|name| !crate::decoration::valid_profile(name))
        {
            return Err("invalid profile name".into());
        }
        options.validate()?;
        Ok(options)
    }

    fn validate(&self) -> Result<(), String> {
        let Some(command) = self.words.first().map(String::as_str) else {
            return Ok(());
        };
        if !COMMANDS.contains(&command) {
            return Err(format!("unknown command {command}; use help"));
        }
        let maximum = if command == "attach" { 2 } else { 1 };
        if self.words.len() > maximum {
            return Err(format!("unexpected argument(s) for {command}"));
        }
        Ok(())
    }

    pub fn legacy(&self) -> Option<PathBuf> {
        (self
            .state
            .file_name()
            .is_some_and(|name| name == "global-state.kdl"))
        .then(|| self.state.with_file_name("state.kdl"))
    }
}

pub fn sessions(options: &Options) -> Result<Vec<Session>, String> {
    let path = if options.state.exists() {
        options.state.clone()
    } else if let Some(legacy) = options.legacy().filter(|path| path.exists()) {
        legacy
    } else {
        return Ok(Vec::new());
    };
    let text = fs::read_to_string(&path).map_err(|error| error.to_string())?;
    let doc: kdl::KdlDocument = text.parse().map_err(|error| format!("state: {error}"))?;
    if doc
        .nodes()
        .iter()
        .any(|node| node.name().value() == "catalog")
    {
        let catalog = Catalog::decode(&text)?;
        catalog
            .sessions
            .iter()
            .map(|id| {
                let path = global::session_path(&options.state, *id);
                let text = fs::read_to_string(&path)
                    .map_err(|error| format!("{}: {error}", path.display()))?;
                session::decode(&text, *id)
            })
            .collect()
    } else {
        Ok(crate::kdl::decode(&text)?.sessions)
    }
}

pub fn run(mut options: Options) -> Result<(), String> {
    if options.repl && !complete(&mut options)? {
        return Ok(());
    }
    let command = options
        .words
        .first()
        .map(String::as_str)
        .unwrap_or("attach");
    match command {
        "help" => print!("{USAGE}"),
        "install" | "update" | "remove" => println!(
            "nui-yalij: {command} will be implemented when the application reaches a more stable release. No changes made."
        ),
        "server" => {
            return crate::server::serve(
                options.state.clone(),
                options.config.clone(),
                options.legacy(),
            );
        }
        "attach" => {
            let target = options.words.get(1).cloned();
            let inside = env::var_os("NUI_YALIJ_STATE")
                .and_then(|path| absolute(Path::new(&path)).ok())
                == Some(options.state.clone());
            if inside {
                let from_session = env::var("NUI_YALIJ_SESSION")
                    .ok()
                    .and_then(|id| id.parse().ok())
                    .ok_or("missing inside-session identity")?;
                let terminal = env::var("NUI_YALIJ_TERMINAL")
                    .ok()
                    .and_then(|id| id.parse().ok())
                    .ok_or("missing inside-terminal identity")?;
                let stream = client::connect(&options.state).map_err(|error| error.to_string())?;
                let result = client::control(
                    stream,
                    Request::Switch {
                        session: target,
                        from_session,
                        terminal,
                        cwd: env::current_dir().ok().map(|dir| dir.display().to_string()),
                    },
                )?;
                println!("{result}");
            } else {
                if !std::io::IsTerminal::is_terminal(&io::stdin()) {
                    return Err("attach needs a terminal".into());
                }
                return client::attach(client::start(&options)?, &options, target);
            }
        }
        "list" => match client::connect(&options.state) {
            Ok(stream) => print!("{}", client::control(stream, Request::List)?),
            Err(_) => print!("{}", app::describe(&sessions(&options)?, &[])),
        },
        "reload" => {
            let stream =
                client::connect(&options.state).map_err(|_| "no server to reload".to_string())?;
            println!("{}", client::control(stream, Request::Reload)?);
        }
        "kill" => match client::connect(&options.state) {
            Ok(stream) => println!(
                "{}",
                client::control(
                    stream,
                    Request::Kill {
                        apply: options.apply
                    }
                )?
            ),
            Err(_) => println!("no server running for {}", options.state.display()),
        },
        _ => unreachable!(),
    }
    Ok(())
}

fn complete(options: &mut Options) -> Result<bool, String> {
    if !std::io::IsTerminal::is_terminal(&io::stdin()) {
        return Err("--repl needs a terminal".into());
    }
    loop {
        if options.words.is_empty() {
            let Some(index) = choose("Command", &COMMANDS.map(str::to_string))? else {
                return Ok(false);
            };
            options.words.push(COMMANDS[index].into());
        }
        if options.words == ["attach"] {
            let mut names: Vec<String> = sessions(options)?
                .into_iter()
                .map(|session| session.name)
                .collect();
            names.push("+ new session".into());
            let Some(index) = choose("Session", &names)? else {
                options.words.clear();
                continue;
            };
            let name = if index + 1 == names.len() {
                prompt("Session name: ")?
            } else {
                names[index].clone()
            };
            if name.is_empty() {
                continue;
            }
            options.words.push(name);
        }
        break;
    }
    options.validate()?;
    println!(
        "nui-yalij --state {:?} --config {:?}{} {}{}",
        options.state,
        options.config,
        options
            .profile
            .as_ref()
            .map(|name| format!(" --profile {name:?}"))
            .unwrap_or_default(),
        options
            .words
            .iter()
            .map(|word| format!("{word:?}"))
            .collect::<Vec<_>>()
            .join(" "),
        if options.words[0] == "kill" {
            " --apply"
        } else {
            ""
        }
    );
    if options.words[0] == "kill" {
        println!("Stops every session server and connected instance in this catalog.");
    }
    let answer = prompt("Apply? (N/y) ")?;
    if answer != "y" {
        println!("Cancelled.");
        return Ok(false);
    }
    options.apply = true;
    Ok(true)
}

fn prompt(label: &str) -> Result<String, String> {
    print!("{label}");
    io::stdout().flush().map_err(|error| error.to_string())?;
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .map_err(|error| error.to_string())?;
    Ok(input.trim().to_string())
}

fn choose(title: &str, choices: &[String]) -> Result<Option<usize>, String> {
    let mut terminal = ratatui::try_init().map_err(|error| error.to_string())?;
    let result = (|| {
        let mut selected = 0;
        loop {
            terminal
                .draw(|frame| {
                    let items: Vec<ListItem> = choices
                        .iter()
                        .map(|choice| ListItem::new(Line::raw(choice)))
                        .collect();
                    let list = List::new(items)
                        .block(Block::bordered().title(format!(
                            "{title} · j/k ↑/↓ · enter choose · esc back/cancel"
                        )))
                        .highlight_style(Style::new().reversed())
                        .highlight_symbol("› ");
                    frame.render_stateful_widget(
                        list,
                        frame.area(),
                        &mut ListState::default().with_selected(Some(selected)),
                    );
                })
                .map_err(|error| error.to_string())?;
            if let Event::Key(key) = event::read().map_err(|error| error.to_string())? {
                if key.kind == KeyEventKind::Release {
                    continue;
                }
                match key.code {
                    KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1) % choices.len(),
                    KeyCode::Up | KeyCode::Char('k') => {
                        selected = (selected + choices.len() - 1) % choices.len()
                    }
                    KeyCode::Enter => return Ok(Some(selected)),
                    KeyCode::Esc => return Ok(None),
                    _ => {}
                }
            }
        }
    })();
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(words: &[&str]) -> Result<Options, String> {
        Options::parse(words.iter().map(|word| word.to_string()).collect())
    }
    #[test]
    fn options_are_explicit() {
        assert!(parse(&["--state"]).is_err());
        assert!(parse(&["--wat"]).is_err());
        assert!(parse(&["list", "extra"]).is_err());
        assert!(parse(&["--profile", "../escape"]).is_err());
        let options = parse(&["kill", "--repl", "--apply", "--profile=work"]).unwrap();
        assert!(options.repl && options.apply);
        assert_eq!(options.profile.as_deref(), Some("work"));
    }
}
