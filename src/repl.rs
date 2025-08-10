use std::collections::HashMap;

use reedline::{
    ColumnarMenu, Completer, Emacs, KeyCode, KeyModifiers, MenuBuilder, Reedline, ReedlineEvent,
    ReedlineMenu, Signal, Suggestion, default_emacs_keybindings,
};

type Action<T> = fn(&clap::ArgMatches, &mut T) -> anyhow::Result<String>;

struct Command<T> {
    clap_representation: clap::Command,
    action: Action<T>,
}

pub struct Repl<T> {
    context: T,
    commands: HashMap<String, Command<T>>,
}

impl<T> Repl<T> {
    pub fn new(context: T) -> Self {
        Self {
            context,
            commands: HashMap::default(),
        }
    }

    pub fn add_command(mut self, command: clap::Command, action: Action<T>) -> Self {
        self.commands.insert(
            command.get_name().to_string(),
            Command {
                clap_representation: command.disable_help_flag(true),
                action,
            },
        );
        Self {
            context: self.context,
            commands: self.commands,
        }
    }

    fn get_help(&self) -> String {
        let mut command = clap::Command::new("Debugito");
        for subcommand in self.commands.values() {
            command = command.subcommand(subcommand.clap_representation.clone());
        }
        command = command.override_usage("[COMMAND] [ARGS]");
        command = command.disable_help_flag(true);
        command.render_help().to_string()
    }

    pub fn run(&mut self) -> anyhow::Result<()> {
        let completer = Box::new(CustomCompleter::new(&self.commands));
        // Use the interactive menu to select options from the completer
        let completion_menu = Box::new(ColumnarMenu::default().with_name("completion_menu"));
        // Set up the required keybindings
        let mut keybindings = default_emacs_keybindings();
        keybindings.add_binding(
            KeyModifiers::NONE,
            KeyCode::Tab,
            ReedlineEvent::UntilFound(vec![
                ReedlineEvent::Menu("completion_menu".to_string()),
                ReedlineEvent::MenuNext,
            ]),
        );

        let edit_mode = Box::new(Emacs::new(keybindings));

        let mut line_editor = Reedline::create()
            .with_completer(completer)
            .with_menu(ReedlineMenu::EngineCompleter(completion_menu))
            .with_edit_mode(edit_mode);
        let prompt = CustomPrompt::new();
        loop {
            let signal = line_editor.read_line(&prompt)?;
            match signal {
                Signal::Success(buffer) => self.run_command(buffer),
                Signal::CtrlD | Signal::CtrlC => {
                    println!("\nAborted!");
                    return Ok(());
                }
            }
        }
    }

    fn run_command(&mut self, buffer: String) {
        let parser = clap::Command::new("app")
            .subcommands(
                self.commands
                    .values()
                    .map(|v| v.clap_representation.clone())
                    .collect::<Vec<clap::Command>>(),
            )
            .no_binary_name(true);
        let matches = parser.try_get_matches_from(buffer.split_whitespace());
        if let Ok(matches) = matches {
            if let Some((command_name, args)) = matches.subcommand() {
                let command = self.commands.get_mut(command_name).unwrap();
                let result = (command.action)(args, &mut self.context);
                match result {
                    Ok(message) => println!("{}\n", message),
                    Err(message) => {
                        println!("{}\n", message);
                        println!("{}", command.clap_representation.render_help());
                    }
                }
            }
        } else {
            println!("{}", self.get_help());
        }
    }
}

struct CustomCompleter {
    commands: Vec<String>,
}

impl CustomCompleter {
    fn new<T>(commands: &HashMap<String, Command<T>>) -> Self {
        Self {
            commands: commands.keys().cloned().collect(),
        }
    }
}

impl Completer for CustomCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        self.commands
            .iter()
            .filter(|command| command.starts_with(line))
            .map(|command| Suggestion {
                value: command.to_string(),
                description: None,
                style: None,
                extra: None,
                span: reedline::Span { start: 0, end: pos },
                append_whitespace: true,
            })
            .collect()
    }
}

struct CustomPrompt {}

impl CustomPrompt {
    fn new() -> Self {
        Self {}
    }
}

impl reedline::Prompt for CustomPrompt {
    fn render_prompt_left(&self) -> std::borrow::Cow<str> {
        std::borrow::Cow::Borrowed("")
    }

    fn render_prompt_right(&self) -> std::borrow::Cow<str> {
        std::borrow::Cow::Borrowed("")
    }

    fn render_prompt_indicator(
        &self,
        _prompt_mode: reedline::PromptEditMode,
    ) -> std::borrow::Cow<str> {
        std::borrow::Cow::Borrowed(">")
    }

    fn render_prompt_multiline_indicator(&self) -> std::borrow::Cow<str> {
        std::borrow::Cow::Borrowed(">>")
    }

    fn render_prompt_history_search_indicator(
        &self,
        _history_search: reedline::PromptHistorySearch,
    ) -> std::borrow::Cow<str> {
        std::borrow::Cow::Borrowed("Search>")
    }
}
