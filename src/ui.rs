use anyhow::Result;
use crossterm::{
    cursor::{Hide, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
#[allow(clippy::wildcard_imports)]
use ratatui::{prelude::*, widgets::*};
use std::{
    cell::RefCell,
    cmp::{max, min},
    io,
    path::PathBuf,
    rc::Rc,
};
use style::palette::tailwind;
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;
use unicode_width::UnicodeWidthStr;

use crate::ssh_config::editor::{self, HostFields, FIELDS};
use crate::{searchable::Searchable, ssh};

const INFO_TEXT: &str =
    "(Esc) quit | (↑) move up | (↓) move down | (enter) select | (ctrl+r) reload | (ctrl+t) add | (ctrl+o) edit";

#[derive(Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct AppConfig {
    pub config_paths: Vec<String>,

    pub search_filter: Option<String>,
    pub color: String,
    pub sort_by_name: bool,
    pub sort_by_score: bool,
    pub show_proxy_command: bool,

    pub command_template: String,
    pub command_template_on_session_start: Option<String>,
    pub command_template_on_session_end: Option<String>,
    pub exit_after_ssh_session_ends: bool,
}

pub struct App {
    config: AppConfig,

    search: Input,

    table_state: TableState,
    hosts: Searchable<ssh::Host>,
    table_columns_constraints: Vec<Constraint>,
    page_step: usize,

    palette: tailwind::Palette,

    form: Option<HostForm>,
    /// One-shot footer message, cleared on the next key press.
    status: Option<String>,
}

/// The add/edit host popup.
struct HostForm {
    /// Block being edited: config file, 0-based `Host` line, row name used to detect a stale file. `None` adds a new host.
    target: Option<(PathBuf, usize, String)>,
    inputs: [Input; 5],
    focused: usize,
    error: Option<String>,
}

#[derive(PartialEq)]
enum AppKeyAction {
    Ok,
    Stop,
    Continue,
}

impl App {
    /// # Errors
    ///
    /// Will return `Err` if the SSH configuration file cannot be parsed.
    pub fn new(config: &AppConfig) -> Result<App> {
        let hosts = load_hosts(config)?;

        let search_input = config.search_filter.clone().unwrap_or_default();

        let mut app = App {
            config: config.clone(),

            search: search_input.clone().into(),

            table_state: TableState::default().with_selected(0),
            table_columns_constraints: Vec::new(),
            page_step: 21,
            palette: palette_by_name(&config.color)?,

            hosts: Searchable::new(config.sort_by_score, hosts, &search_input),
            form: None,
            status: None,
        };
        app.calculate_table_columns_constraints();

        Ok(app)
    }

    /// # Errors
    ///
    /// Will return `Err` if the terminal cannot be configured.
    pub fn start(&mut self) -> Result<()> {
        let stdout = io::stdout().lock();
        let backend = CrosstermBackend::new(stdout);
        let terminal = Rc::new(RefCell::new(Terminal::new(backend)?));

        setup_terminal(&terminal)?;

        // create app and run it
        let res = self.run(&terminal);

        restore_terminal(&terminal)?;

        if let Err(err) = res {
            println!("{err:?}");
        }

        Ok(())
    }

    fn run<B>(&mut self, terminal: &Rc<RefCell<Terminal<B>>>) -> Result<()>
    where
        B: Backend + std::io::Write,
        <B as Backend>::Error: Send + Sync + 'static,
    {
        loop {
            terminal.borrow_mut().draw(|f| ui(f, self))?;

            let ev = event::read()?;

            if let Event::Key(key) = ev {
                if self.form.is_some() {
                    self.on_form_key(key);
                    continue;
                }
                if key.kind == KeyEventKind::Press {
                    let action = self.on_key_press(terminal, key)?;
                    match action {
                        AppKeyAction::Ok => continue,
                        AppKeyAction::Stop => break,
                        AppKeyAction::Continue => {}
                    }
                }

                self.handle_search_event(&ev);
            }
        }

        Ok(())
    }

    fn on_key_press<B>(
        &mut self,
        terminal: &Rc<RefCell<Terminal<B>>>,
        key: KeyEvent,
    ) -> Result<AppKeyAction>
    where
        B: Backend + std::io::Write,
        <B as Backend>::Error: Send + Sync + 'static,
    {
        #[allow(clippy::enum_glob_use)]
        use KeyCode::*;

        self.status = None;

        let is_ctrl_pressed = key.modifiers.contains(KeyModifiers::CONTROL);

        if is_ctrl_pressed {
            let action = self.on_key_press_ctrl(key);
            if action != AppKeyAction::Continue {
                return Ok(action);
            }
        }

        match key.code {
            Esc => return Ok(AppKeyAction::Stop),
            Down => self.next(),
            Up => self.previous(),
            Home => self.table_state.select(Some(0)),
            End => self
                .table_state
                .select(Some(self.hosts.len().saturating_sub(1))),
            PageDown => {
                let i = self.table_state.selected().unwrap_or(0);
                let target = min(
                    i.saturating_add(self.page_step),
                    self.hosts.len().saturating_sub(1),
                );

                self.table_state.select(Some(target));
            }
            PageUp => {
                let i = self.table_state.selected().unwrap_or(0);
                let target = max(i.saturating_sub(self.page_step), 0);

                self.table_state.select(Some(target));
            }
            Enter => {
                let selected = self.table_state.selected().unwrap_or(0);
                if selected >= self.hosts.len() {
                    return Ok(AppKeyAction::Ok);
                }

                let host: &ssh::Host = &self.hosts[selected];

                restore_terminal(terminal).expect("Failed to restore terminal");

                if let Some(template) = &self.config.command_template_on_session_start {
                    host.spawn_command_template(template)?;
                }

                host.spawn_command_template(&self.config.command_template)?;

                if let Some(template) = &self.config.command_template_on_session_end {
                    host.spawn_command_template(template)?;
                }

                setup_terminal(terminal).expect("Failed to setup terminal");

                if self.config.exit_after_ssh_session_ends {
                    return Ok(AppKeyAction::Stop);
                }
            }
            _ => return Ok(AppKeyAction::Continue),
        }

        Ok(AppKeyAction::Ok)
    }

    fn on_key_press_ctrl(&mut self, key: KeyEvent) -> AppKeyAction {
        #[allow(clippy::enum_glob_use)]
        use KeyCode::*;

        match key.code {
            Char('c') => AppKeyAction::Stop,
            Char('j' | 'n') => {
                self.next();
                AppKeyAction::Ok
            }
            Char('k' | 'p') => {
                self.previous();
                AppKeyAction::Ok
            }
            Char('r') => {
                self.reload_hosts();
                AppKeyAction::Ok
            }
            Char('t') => {
                self.open_add_form();
                AppKeyAction::Ok
            }
            Char('o') => {
                self.open_edit_form();
                AppKeyAction::Ok
            }
            _ => AppKeyAction::Continue,
        }
    }

    fn open_add_form(&mut self) {
        self.form = Some(HostForm {
            target: None,
            inputs: std::array::from_fn(|_| Input::default()),
            focused: 0,
            error: None,
        });
    }

    fn open_edit_form(&mut self) {
        let selected = self.table_state.selected().unwrap_or(0);
        if selected >= self.hosts.len() {
            return;
        }
        let host = self.hosts[selected].clone();

        let Some((path, line)) = host.source else {
            self.status = Some(format!(
                "Can't edit {}: its config file is unknown.",
                host.name
            ));
            return;
        };

        match editor::read_block(&path, line, &host.name) {
            Ok(fields) => {
                self.form = Some(HostForm {
                    target: Some((path, line, host.name)),
                    inputs: fields.map(Input::from),
                    focused: 0,
                    error: None,
                });
            }
            Err(err) => self.status = Some(format!("Can't edit {}: {err}", path.display())),
        }
    }

    fn on_form_key(&mut self, key: KeyEvent) {
        let Some(form) = self.form.as_mut() else {
            return;
        };

        if key.kind == KeyEventKind::Press {
            match key.code {
                KeyCode::Esc => {
                    self.form = None;
                    return;
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.form = None;
                    return;
                }
                KeyCode::Tab | KeyCode::Down => {
                    form.focused = (form.focused + 1) % FIELDS.len();
                    return;
                }
                KeyCode::BackTab | KeyCode::Up => {
                    form.focused = (form.focused + FIELDS.len() - 1) % FIELDS.len();
                    return;
                }
                KeyCode::Enter => {
                    self.save_form();
                    return;
                }
                _ => {}
            }
        }

        form.inputs[form.focused].handle_event(&Event::Key(key));
    }

    fn save_form(&mut self) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let fields: HostFields = std::array::from_fn(|i| form.inputs[i].value().trim().to_string());

        let invalid = if fields[0].is_empty() {
            Some("Host must not be empty.")
        } else if !fields[3].is_empty() && !matches!(fields[3].parse::<u16>(), Ok(1..=u16::MAX)) {
            Some("Port must be a number from 1 to 65535.")
        } else {
            None
        };
        if let Some(message) = invalid {
            form.error = Some(message.to_string());
            return;
        }

        let (path, result) = if let Some((path, line, name)) = &form.target {
            (
                path.clone(),
                editor::write_block(path, *line, name, &fields),
            )
        } else {
            let Some(raw) = self.config.config_paths.last() else {
                form.error = Some("No config file to add the host to.".to_string());
                return;
            };
            let path = PathBuf::from(shellexpand::tilde(raw).as_ref());
            let result = editor::append_block(&path, &fields);
            (path, result)
        };

        match result {
            Ok(()) => {
                self.form = None;
                self.reload_hosts();
                self.status = Some(format!("Saved to {}", path.display()));
            }
            Err(err) => form.error = Some(format!("Can't save to {}: {err}", path.display())),
        }
    }

    /// Updates the search input from a terminal event, re-filters the host
    /// list, and keeps the table selection valid.
    ///
    /// When the search text actually changes, the selection resets to the
    /// top result instead of keeping its previous numeric index: the old
    /// index could otherwise point at an unrelated host in the newly
    /// filtered list, making an apparently-unmatched host look selected.
    fn handle_search_event(&mut self, ev: &Event) {
        let search_value_before = self.search.value().to_string();
        self.search.handle_event(ev);

        if self.search.value() == search_value_before {
            let selected = self.table_state.selected().unwrap_or(0);
            if selected >= self.hosts.len() {
                self.table_state.select(Some(match self.hosts.len() {
                    0 => 0,
                    _ => self.hosts.len() - 1,
                }));
            }
        } else {
            self.hosts.search(self.search.value());
            self.table_state.select(Some(0));
        }
    }

    /// Re-reads the configuration files. Keeps the current list when a file
    /// no longer parses.
    fn reload_hosts(&mut self) {
        if let Ok(hosts) = load_hosts(&self.config) {
            self.hosts = Searchable::new(self.config.sort_by_score, hosts, self.search.value());
            self.table_state.select(Some(0));
            self.calculate_table_columns_constraints();
        }
    }

    fn next(&mut self) {
        let i = match self.table_state.selected() {
            Some(i) => {
                if self.hosts.is_empty() || i >= self.hosts.len() - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    fn previous(&mut self) {
        let i = match self.table_state.selected() {
            Some(i) => {
                if self.hosts.is_empty() {
                    0
                } else if i == 0 {
                    self.hosts.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    fn calculate_table_columns_constraints(&mut self) {
        let mut lengths = Vec::new();

        let name_len = self
            .hosts
            .non_filtered_iter()
            .map(|d| d.name.as_str())
            .map(UnicodeWidthStr::width)
            .max()
            .unwrap_or(0);
        lengths.push(name_len);

        let aliases_len = self
            .hosts
            .non_filtered_iter()
            .map(|d| d.aliases.as_str())
            .map(UnicodeWidthStr::width)
            .max()
            .unwrap_or(0);
        lengths.push(aliases_len);

        let user_len = self
            .hosts
            .non_filtered_iter()
            .map(|d| match &d.user {
                Some(user) => user.as_str(),
                None => "",
            })
            .map(UnicodeWidthStr::width)
            .max()
            .unwrap_or(0);
        lengths.push(user_len);

        let destination_len = self
            .hosts
            .non_filtered_iter()
            .map(|d| d.destination.as_str())
            .map(UnicodeWidthStr::width)
            .max()
            .unwrap_or(0);
        lengths.push(destination_len);

        let port_len = self
            .hosts
            .non_filtered_iter()
            .map(|d| match &d.port {
                Some(port) => port.as_str(),
                None => "",
            })
            .map(UnicodeWidthStr::width)
            .max()
            .unwrap_or(0);
        lengths.push(port_len);

        if self.config.show_proxy_command {
            let proxy_len = self
                .hosts
                .non_filtered_iter()
                .map(|d| match &d.proxy_command {
                    Some(proxy) => proxy.as_str(),
                    None => "",
                })
                .map(UnicodeWidthStr::width)
                .max()
                .unwrap_or(0);
            lengths.push(proxy_len);
        }

        let mut new_constraints = vec![
            // +1 for padding
            Constraint::Length(u16::try_from(lengths[0]).unwrap_or_default() + 1),
        ];
        new_constraints.extend(
            lengths
                .iter()
                .skip(1)
                .map(|len| Constraint::Min(u16::try_from(*len).unwrap_or_default() + 1)),
        );

        self.table_columns_constraints = new_constraints;
    }
}

fn load_hosts(config: &AppConfig) -> Result<Vec<ssh::Host>> {
    let mut hosts = Vec::new();

    for path in &config.config_paths {
        let parsed_hosts = match ssh::parse_config(path) {
            Ok(hosts) => hosts,
            Err(err) => {
                if let ssh::ParseConfigError::Io(io_err) = &err {
                    if io_err.kind() == std::io::ErrorKind::NotFound {
                        if path == "/etc/ssh/ssh_config" {
                            // Ignore missing system-wide SSH configuration file
                            continue;
                        }

                        anyhow::bail!(
                            "SSH configuration file not found: {path}\nCreate it, or pass a different path with -c/--config."
                        );
                    }
                }

                anyhow::bail!("Failed to parse SSH configuration file: {err:?}");
            }
        };

        hosts.extend(parsed_hosts);
    }

    if config.sort_by_name {
        hosts.sort_by_key(|host| host.name.to_lowercase());
    }

    Ok(hosts)
}

fn palette_by_name(name: &str) -> Result<tailwind::Palette> {
    let palette = match name.to_lowercase().as_str() {
        "slate" => tailwind::SLATE,
        "gray" => tailwind::GRAY,
        "zinc" => tailwind::ZINC,
        "neutral" => tailwind::NEUTRAL,
        "stone" => tailwind::STONE,
        "red" => tailwind::RED,
        "orange" => tailwind::ORANGE,
        "amber" => tailwind::AMBER,
        "yellow" => tailwind::YELLOW,
        "lime" => tailwind::LIME,
        "green" => tailwind::GREEN,
        "emerald" => tailwind::EMERALD,
        "teal" => tailwind::TEAL,
        "cyan" => tailwind::CYAN,
        "sky" => tailwind::SKY,
        "blue" => tailwind::BLUE,
        "indigo" => tailwind::INDIGO,
        "violet" => tailwind::VIOLET,
        "purple" => tailwind::PURPLE,
        "fuchsia" => tailwind::FUCHSIA,
        "pink" => tailwind::PINK,
        "rose" => tailwind::ROSE,
        _ => anyhow::bail!(
            "Unknown color: {name}\nValid colors: slate, gray, zinc, neutral, stone, red, orange, amber, yellow, lime, green, emerald, teal, cyan, sky, blue, indigo, violet, purple, fuchsia, pink, rose"
        ),
    };

    Ok(palette)
}

fn setup_terminal<B>(terminal: &Rc<RefCell<Terminal<B>>>) -> Result<()>
where
    B: Backend + std::io::Write,
    <B as Backend>::Error: Send + Sync + 'static,
{
    let mut terminal = terminal.borrow_mut();

    // setup terminal
    enable_raw_mode()?;
    execute!(terminal.backend_mut(), Hide, EnterAlternateScreen)?;

    Ok(())
}

fn restore_terminal<B>(terminal: &Rc<RefCell<Terminal<B>>>) -> Result<()>
where
    B: Backend + std::io::Write,
    <B as Backend>::Error: Send + Sync + 'static,
{
    let mut terminal = terminal.borrow_mut();
    terminal.clear()?;

    // restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), Show, LeaveAlternateScreen)?;

    Ok(())
}

fn ui(f: &mut Frame, app: &mut App) {
    let rects = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(5),
        Constraint::Length(3),
    ])
    .split(f.area());

    render_searchbar(f, app, rects[0]);

    render_table(f, app, rects[1]);

    render_footer(f, app, rects[2]);

    if let Some(form) = &app.form {
        let pos = render_form(f, app, form);
        f.set_cursor_position(pos);
    } else {
        let mut cursor_position = rects[0].as_position();
        cursor_position.x += u16::try_from(app.search.cursor()).unwrap_or_default() + 4;
        cursor_position.y += 1;

        f.set_cursor_position(cursor_position);
    }
}

fn render_searchbar(f: &mut Frame, app: &mut App, area: Rect) {
    let info_footer = Paragraph::new(Line::from(app.search.value())).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(app.palette.c400))
            .border_type(BorderType::Rounded)
            .padding(Padding::horizontal(3)),
    );
    f.render_widget(info_footer, area);
}

fn render_table(f: &mut Frame, app: &mut App, area: Rect) {
    // The visible row count: the area minus the two border lines and the header.
    app.page_step = max(usize::from(area.height.saturating_sub(3)), 1);

    let header_style = Style::default().fg(tailwind::CYAN.c500);
    let selected_style = Style::default().add_modifier(Modifier::REVERSED);

    let mut header_names = vec!["Name", "Aliases", "User", "Destination", "Port"];
    if app.config.show_proxy_command {
        header_names.push("Proxy");
    }

    let header = header_names
        .iter()
        .copied()
        .map(Cell::from)
        .collect::<Row>()
        .style(header_style)
        .height(1);

    let rows = app.hosts.iter().map(|host| {
        let mut content = vec![
            host.name.clone(),
            host.aliases.clone(),
            host.user.clone().unwrap_or_default(),
            host.destination.clone(),
            host.port.clone().unwrap_or_default(),
        ];
        if app.config.show_proxy_command {
            content.push(host.proxy_command.clone().unwrap_or_default());
        }

        content
            .iter()
            .map(|content| Cell::from(Text::from(content.clone())))
            .collect::<Row>()
    });

    let bar = " █ ";
    let t = Table::new(rows, app.table_columns_constraints.clone())
        .header(header)
        .row_highlight_style(selected_style)
        .highlight_symbol(Text::from(vec![
            "".into(),
            bar.into(),
            bar.into(),
            "".into(),
        ]))
        .highlight_spacing(HighlightSpacing::Always)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::new().fg(app.palette.c400))
                .border_type(BorderType::Rounded),
        );

    f.render_stateful_widget(t, area, &mut app.table_state);
}

fn render_footer(f: &mut Frame, app: &mut App, area: Rect) {
    let text = app.status.as_deref().unwrap_or(INFO_TEXT);
    let info_footer = Paragraph::new(Line::from(text)).centered().block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(app.palette.c400))
            .border_type(BorderType::Rounded),
    );
    f.render_widget(info_footer, area);
}

fn render_form(f: &mut Frame, app: &App, form: &HostForm) -> Position {
    // 2 border rows, 5 fields, 1 spacer, 1 error row.
    let area = f
        .area()
        .centered(Constraint::Max(80), Constraint::Length(9));

    let title = match &form.target {
        Some((path, _, _)) => format!(" Edit host in {} ", path.display()),
        None => format!(
            " Add host to {} ",
            app.config.config_paths.last().map_or("", String::as_str)
        ),
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(app.palette.c400))
        .title(title)
        .title_bottom(" (Tab/↑↓) switch field | (Enter) save | (Esc) cancel ");

    let rows = Layout::vertical([Constraint::Length(1); 7]).split(block.inner(area));
    f.render_widget(Clear, area);
    f.render_widget(block, area);

    let mut cursor = Position::default();
    for (i, input) in form.inputs.iter().enumerate() {
        let [label_area, value_area] =
            Layout::horizontal([Constraint::Length(14), Constraint::Min(1)]).areas(rows[i]);

        let label_style = if i == form.focused {
            Style::new()
                .fg(app.palette.c400)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        };
        f.render_widget(Paragraph::new(FIELDS[i].1).style(label_style), label_area);

        let width = value_area.width.max(1) - 1;
        let scroll = input.visual_scroll(usize::from(width));
        f.render_widget(
            Paragraph::new(input.value()).scroll((0, u16::try_from(scroll).unwrap_or_default())),
            value_area,
        );

        if i == form.focused {
            cursor = Position::new(
                value_area.x
                    + u16::try_from(input.visual_cursor().saturating_sub(scroll))
                        .unwrap_or_default(),
                value_area.y,
            );
        }
    }

    f.render_widget(
        Paragraph::new(form.error.as_deref().unwrap_or(""))
            .style(Style::new().fg(tailwind::RED.c400)),
        rows[6],
    );

    cursor
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> AppConfig {
        AppConfig {
            config_paths: vec![crate::test_support::testdata("search_selection.conf")
                .to_string_lossy()
                .into_owned()],
            search_filter: None,
            color: "blue".to_string(),
            sort_by_name: false,
            sort_by_score: false,
            show_proxy_command: false,
            command_template: r#"ssh "{{{name}}}""#.to_string(),
            command_template_on_session_start: None,
            command_template_on_session_end: None,
            exit_after_ssh_session_ends: false,
        }
    }

    fn type_char(app: &mut App, c: char) {
        let ev = Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        app.handle_search_event(&ev);
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_form_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn type_form(app: &mut App, s: &str) {
        for c in s.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    #[test]
    fn test_add_host_form_writes_config_and_reloads() {
        let path = std::env::temp_dir().join("sshs_test_add_form.conf");
        std::fs::write(&path, "Host first\n  HostName first.example.com\n").unwrap();

        let mut config = test_config();
        config.config_paths = vec![path.to_string_lossy().into_owned()];
        let mut app = App::new(&config).unwrap();

        app.on_key_press_ctrl(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        type_form(&mut app, "second");
        press(&mut app, KeyCode::Tab);
        type_form(&mut app, "second.example.com");
        press(&mut app, KeyCode::Enter);

        let content = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert!(app.form.is_none());
        assert_eq!(app.hosts.len(), 2);
        assert_eq!(
            content,
            "Host first\n  HostName first.example.com\n\nHost second\n  HostName second.example.com\n"
        );
    }

    #[test]
    fn test_host_form_rejects_invalid_port() {
        let mut app = App::new(&test_config()).unwrap();

        app.on_key_press_ctrl(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        type_form(&mut app, "x");
        for _ in 0..3 {
            press(&mut app, KeyCode::Tab);
        }
        type_form(&mut app, "abc");
        press(&mut app, KeyCode::Enter);

        assert_eq!(
            app.form.as_ref().unwrap().error.as_deref(),
            Some("Port must be a number from 1 to 65535.")
        );
    }

    #[test]
    fn test_reload_hosts_picks_up_file_changes() {
        let path = std::env::temp_dir().join("sshs_test_reload.conf");
        std::fs::write(&path, "Host first\n  Hostname first.example.com\n").unwrap();

        let mut config = test_config();
        config.config_paths = vec![path.to_string_lossy().into_owned()];

        let mut app = App::new(&config).unwrap();
        assert_eq!(app.hosts.len(), 1);

        std::fs::write(
            &path,
            "Host first\n  Hostname first.example.com\nHost second\n  Hostname second.example.com\n",
        )
        .unwrap();
        app.reload_hosts();
        std::fs::remove_file(&path).ok();

        assert_eq!(app.hosts.len(), 2);
        assert_eq!(app.table_state.selected(), Some(0));
    }

    #[test]
    fn test_table_columns_constraints_are_computed() {
        let app = App::new(&test_config()).unwrap();

        assert_eq!(app.table_columns_constraints.len(), 5);
        assert!(matches!(
            app.table_columns_constraints[0],
            Constraint::Length(len) if len > 1
        ));
    }

    /// Regression test for <https://github.com/quantumsheep/sshs/issues/120>:
    /// a missing SSH config file used to surface a raw `Io(Os { .. })` debug
    /// error; it should now explain what's missing and how to fix it.
    #[test]
    fn test_missing_config_file_gives_actionable_error() {
        let mut config = test_config();
        config.config_paths = vec!["/nonexistent/path/to/config".to_string()];

        let message = match App::new(&config) {
            Ok(_) => panic!("expected App::new to fail for a missing config file"),
            Err(err) => err.to_string(),
        };

        assert!(
            message.contains("/nonexistent/path/to/config"),
            "error should mention the missing path: {message}"
        );
        assert!(
            !message.contains("Os {"),
            "error should not leak a raw Debug-formatted io::Error: {message}"
        );
    }

    /// Regression test for <https://github.com/quantumsheep/sshs/issues/154>:
    /// typing a search query used to keep the previously selected row index,
    /// which could point at an unrelated host once the list was refiltered.
    #[test]
    fn test_search_resets_selection_to_top_match() {
        let config = test_config();
        let mut app = App::new(&config).unwrap();

        // Sanity check: all 4 hosts are listed in file order before searching.
        assert_eq!(app.hosts.len(), 4);

        // Select the 2nd row ("other"), which won't match the search below.
        app.next();
        assert_eq!(app.table_state.selected(), Some(1));

        for c in "match".chars() {
            type_char(&mut app, c);
        }

        assert_eq!(app.hosts.len(), 3);
        assert_eq!(app.hosts.iter().next().unwrap().name, "match1");

        // The stale index (1) would previously stay selected, highlighting
        // "match2" instead of resetting to the top match "match1".
        assert_eq!(app.table_state.selected(), Some(0));
    }
}
