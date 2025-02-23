use std::collections::HashMap;

use reedline::{Reedline, Signal};

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
                clap_representation: command,
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
        command.render_help().to_string()
    }

    pub fn run(&mut self) -> anyhow::Result<()> {
        let mut line_editor = Reedline::create();
        let prompt = CustomPrompt::new();
        loop {
            let signal = line_editor.read_line(&prompt)?;
            match signal {
                Signal::Success(buffer) => {
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
                        if let Some((subcommand, args)) = matches.subcommand() {
                            let result =
                                (self.commands[subcommand].action)(args, &mut self.context)
                                    .unwrap();
                            println!("{}", result);
                        }
                    } else {
                        println!("{}", self.get_help());
                    }
                }
                Signal::CtrlD | Signal::CtrlC => {
                    println!("\nAborted!");
                    return Ok(());
                }
            }
        }
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
