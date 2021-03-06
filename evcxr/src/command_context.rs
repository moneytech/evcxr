// Copyright 2020 The Evcxr Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::errors::{bail, CompilationError, Error};
use crate::{
    code_block::{CodeBlock, CodeKind},
    eval_context::EvalCallbacks,
    rust_analyzer::Completions,
    EvalContext, EvalContextOutputs, EvalOutputs,
};
use anyhow::Result;

/// A higher level interface to EvalContext. A bit closer to a Repl. Provides commands (start with
/// ':') that alter context state or print information.
pub struct CommandContext {
    print_timings: bool,
    eval_context: EvalContext,
    last_errors: Vec<CompilationError>,
}

impl CommandContext {
    pub fn new() -> Result<(CommandContext, EvalContextOutputs), Error> {
        let (eval_context, eval_context_outputs) = EvalContext::new()?;
        let command_context = CommandContext::with_eval_context(eval_context);
        Ok((command_context, eval_context_outputs))
    }

    pub fn with_eval_context(eval_context: EvalContext) -> CommandContext {
        CommandContext {
            print_timings: false,
            eval_context,
            last_errors: Vec::new(),
        }
    }

    #[doc(hidden)]
    pub fn new_for_testing() -> (CommandContext, EvalContextOutputs) {
        let (eval_context, outputs) = EvalContext::new_for_testing();
        (Self::with_eval_context(eval_context), outputs)
    }

    pub fn execute(&mut self, to_run: &str) -> Result<EvalOutputs, Error> {
        self.execute_with_callbacks(to_run, &mut EvalCallbacks::default())
    }

    pub fn execute_with_callbacks(
        &mut self,
        to_run: &str,
        callbacks: &mut EvalCallbacks,
    ) -> Result<EvalOutputs, Error> {
        use std::time::Instant;
        let mut eval_outputs = EvalOutputs::new();
        let start = Instant::now();
        let mut non_command_code = CodeBlock::new();
        for segment in CodeBlock::new().original_user_code(to_run).segments {
            if let CodeKind::Command(command) = &segment.kind {
                eval_outputs.merge(self.process_command(&command.command, &command.args)?);
            } else {
                non_command_code = non_command_code.with_segment(segment);
            }
        }
        let result = if non_command_code.is_empty() {
            Ok(EvalOutputs::new())
        } else {
            self.eval_context
                .eval_with_callbacks(non_command_code, callbacks)
        };
        let duration = start.elapsed();
        match result {
            Ok(m) => {
                eval_outputs.merge(m);
                if self.print_timings {
                    eval_outputs.timing = Some(duration);
                }
                Ok(eval_outputs)
            }
            Err(Error::CompilationErrors(errors)) => {
                self.last_errors = errors.clone();
                Err(Error::CompilationErrors(errors))
            }
            x => x,
        }
    }

    pub fn set_opt_level(&mut self, level: &str) -> Result<(), Error> {
        self.eval_context.set_opt_level(level)
    }

    /// Returns completions within `src` at `position`, which should be a byte offset. Note, this
    /// function requires &mut self because it mutates internal state in order to determine
    /// completions. It also assumes exclusive access to those resources. However there should be
    /// any visible side effects.
    pub fn completions(&mut self, src: &str, position: usize) -> Result<Completions> {
        let mut non_command_code = CodeBlock::new();
        for segment in CodeBlock::new().original_user_code(src).segments {
            if let CodeKind::Command(command) = &segment.kind {
                if command.command == ":dep" {
                    // Best-effort. If anything goes wrong here, just ignore it.
                    let _ = self.process_dep_command(&command.args);
                    let _ = self.eval_context.write_cargo_toml();
                }
            } else {
                non_command_code = non_command_code.with_segment(segment);
            }
        }
        self.eval_context.completions(non_command_code, position)
    }

    fn load_config(&mut self) -> Result<EvalOutputs, Error> {
        let mut outputs = EvalOutputs::new();
        if let Some(config_dir) = crate::config_dir() {
            let config_file = config_dir.join("init.evcxr");
            if config_file.exists() {
                println!("Loading startup commands from {:?}", config_file);
                let contents = std::fs::read_to_string(config_file)?;
                for line in contents.lines() {
                    outputs.merge(self.execute(line)?);
                }
            }
            // Note: Loaded *after* init.evcxr so that it can access `:dep`s (or
            // any other state changed by :commands) specified in the init file.
            let prelude_file = config_dir.join("prelude.rs");
            if prelude_file.exists() {
                println!("Executing prelude from {:?}", prelude_file);
                let prelude = std::fs::read_to_string(prelude_file)?;
                outputs.merge(self.execute(&prelude)?);
            }
        }
        Ok(outputs)
    }

    fn process_command(
        &mut self,
        command: &str,
        args: &Option<String>,
    ) -> Result<EvalOutputs, Error> {
        match command {
            ":internal_debug" => {
                let debug_mode = !self.eval_context.debug_mode();
                self.eval_context.set_debug_mode(debug_mode);
                text_output(format!("Internals debugging: {}", debug_mode))
            }
            ":load_config" => self.load_config(),
            ":version" => text_output(env!("CARGO_PKG_VERSION")),
            ":vars" => {
                let mut outputs = EvalOutputs::new();
                outputs
                    .content_by_mime_type
                    .insert("text/plain".to_owned(), self.vars_as_text());
                outputs
                    .content_by_mime_type
                    .insert("text/html".to_owned(), self.vars_as_html());
                Ok(outputs)
            }
            ":preserve_vars_on_panic" => {
                self.eval_context.preserve_vars_on_panic =
                    args.as_ref().map(String::as_str) == Some("1");
                text_output(format!(
                    "Preserve vars on panic: {}",
                    self.eval_context.preserve_vars_on_panic
                ))
            }
            ":clear" => self.eval_context.clear().map(|_| EvalOutputs::new()),
            ":dep" => self.process_dep_command(args),
            ":last_compile_dir" => {
                text_output(format!("{:?}", self.eval_context.last_compile_dir()))
            }
            ":opt" => {
                let new_level = if let Some(n) = args {
                    &n
                } else if self.eval_context.opt_level() == "2" {
                    "0"
                } else {
                    "2"
                };
                self.eval_context.set_opt_level(new_level)?;
                text_output(format!("Optimization: {}", self.eval_context.opt_level()))
            }
            ":fmt" => {
                let new_format = if let Some(f) = args { f } else { "{:?}" };
                self.eval_context.set_output_format(new_format.to_owned());
                text_output(format!(
                    "Output format: {}",
                    self.eval_context.output_format()
                ))
            }
            ":efmt" => {
                if let Some(f) = args {
                    self.eval_context.set_error_format(f)?;
                }
                text_output(format!(
                    "Error format: {} (errors must implement {})",
                    self.eval_context.error_format(),
                    self.eval_context.error_format_trait()
                ))
            }
            ":quit" => std::process::exit(0),
            ":timing" => {
                self.print_timings = !self.print_timings;
                text_output(format!("Timing: {}", self.print_timings))
            }
            ":time_passes" => {
                self.eval_context
                    .set_time_passes(!self.eval_context.time_passes());
                text_output(format!("Time passes: {}", self.eval_context.time_passes()))
            }
            ":sccache" => {
                self.eval_context
                    .set_sccache(args.as_ref().map(String::as_str) != Some("0"))?;
                text_output(format!("sccache: {}", self.eval_context.sccache()))
            }
            ":linker" => {
                if let Some(linker) = args {
                    self.eval_context.set_linker(linker.to_owned());
                }
                text_output(format!("linker: {}", self.eval_context.linker()))
            }
            ":explain" => {
                if self.last_errors.is_empty() {
                    bail!("No last error to explain");
                } else {
                    let mut all_explanations = String::new();
                    for error in &self.last_errors {
                        if let Some(explanation) = error.explanation() {
                            all_explanations.push_str(explanation);
                        } else {
                            bail!("Sorry, last error has no explanation");
                        }
                    }
                    text_output(all_explanations)
                }
            }
            ":last_error_json" => {
                let mut errors_out = String::new();
                for error in &self.last_errors {
                    use std::fmt::Write;
                    write!(&mut errors_out, "{}", error.json)?;
                    errors_out.push('\n');
                }
                bail!(errors_out);
            }
            ":help" => text_output(
                ":vars             List bound variables and their types\n\
                 :opt [level]      Toggle/set optimization level\n\
                 :fmt [format]     Set output formatter (default: {:?}). \n\
                 :efmt [format]    Set the formatter for errors returned by ?\n\
                 :explain          Print explanation of last error\n\
                 :clear            Clear all state, keeping compilation cache\n\
                 :dep              Add dependency. e.g. :dep regex = \"1.0\"\n\
                 :sccache [0|1]    Set whether to use sccache.\n\
                 :linker [linker]  Set/print linker. Supported: system, lld\n\
                 :version          Print Evcxr version\n\
                 :quit             Quit evaluation and exit\n\
                 :preserve_vars_on_panic [0|1]  Try to keep vars on panic\n\n\
                 Mostly for development / debugging purposes:\n\
                 :last_compile_dir Print the directory in which we last compiled\n\
                 :timing           Toggle printing of how long evaluations take\n\
                 :last_error_json  Print the last compilation error as JSON (for debugging)\n\
                 :time_passes      Toggle printing of rustc pass times (requires nightly)\n\
                 :internal_debug   Toggle various internal debugging code",
            ),
            _ => bail!("Unrecognised command {}", command),
        }
    }

    fn vars_as_text(&self) -> String {
        let mut out = String::new();
        for (var, ty) in self.eval_context.variables_and_types() {
            out.push_str(var);
            out.push_str(": ");
            out.push_str(ty);
            out.push_str("\n");
        }
        out
    }

    fn vars_as_html(&self) -> String {
        let mut out = String::new();
        out.push_str("<table><tr><th>Variable</th><th>Type</th></tr>");
        for (var, ty) in self.eval_context.variables_and_types() {
            out.push_str("<tr><td>");
            html_escape(var, &mut out);
            out.push_str("</td><td>");
            html_escape(ty, &mut out);
            out.push_str("</td><tr>");
        }
        out.push_str("</table>");
        out
    }

    fn process_dep_command(&mut self, args: &Option<String>) -> Result<EvalOutputs, Error> {
        use regex::Regex;
        let args = if let Some(v) = args {
            v
        } else {
            bail!(":dep requires arguments")
        };
        lazy_static! {
            static ref DEP_RE: Regex = Regex::new("^([^= ]+) *(= *(.+))?$").unwrap();
        }
        if let Some(captures) = DEP_RE.captures(args) {
            self.eval_context.add_dep(
                &captures[1],
                &captures.get(3).map_or("\"*\"", |m| m.as_str()),
            )?;
            Ok(EvalOutputs::new())
        } else {
            bail!("Invalid :dep command. Expected: name = ... or just name");
        }
    }
}

fn html_escape(input: &str, out: &mut String) {
    for ch in input.chars() {
        match ch {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            x => out.push(x),
        }
    }
}

fn text_output<T: Into<String>>(text: T) -> Result<EvalOutputs, Error> {
    let mut outputs = EvalOutputs::new();
    let mut content = text.into();
    content.push('\n');
    outputs
        .content_by_mime_type
        .insert("text/plain".to_owned(), content);
    Ok(outputs)
}
