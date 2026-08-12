//! The toolbox as the agent loop sees it.
//!
//! The loop knows nothing about themes or layauts — it asks a
//! [`ToolRegistry`] which tools exist, what the machine looks like,
//! whether a call would change anything, and what happened when it ran.
//! This is where the tools answer.
//!
//! The impl lives here rather than beside the trait so that the agent
//! module stays what it is: a loop that would drive any set of tools.
//! Nothing in it should have to know that a desktop exists.
//!
//! [`ToolRegistry::effect`] is the load-bearing part. It is asked
//! BEFORE anything runs, and it decides whether the user is asked to
//! approve. Answering [`Effect::Read`] for a tool that writes would
//! quietly disable the approval path, so the answer is derived from the
//! same tool names the writing tools are dispatched under and there is
//! a test that fails if a writing tool is ever missing from the list.

use crate::agent::registry::{Change, Effect, EnvironmentFact, ToolOutput, ToolRegistry};
use crate::message::{ToolCall, ToolDeclaration};
use crate::tools::{catalog, Toolbox};
use crate::tools::{
    BUILTIN_THEMES_NOTE, BUILTIN_WIDGETS_NOTE, TOOL_SET_CONFIG, TOOL_SET_LAYAUT, TOOL_SET_THEME,
};

/// Every tool that writes. A tool named here is put to the user before
/// it runs; a tool not named here never is.
const WRITING_TOOLS: [&str; 3] = [TOOL_SET_THEME, TOOL_SET_LAYAUT, TOOL_SET_CONFIG];

impl ToolRegistry for Toolbox {
    fn declarations(&self) -> Vec<ToolDeclaration> {
        Toolbox::declarations(self)
    }

    /// What is installed, for the system prompt.
    ///
    /// Deliberately no "the theme is currently X": these facts are read
    /// once when a session starts and the prompt then stays
    /// byte-identical so the provider can cache it. A current value
    /// frozen into that prompt would still say "crimson" after the
    /// agent had itself changed it to something else. Which one is in
    /// force is what the listing tools answer, freshly, when asked.
    fn environment(&self) -> Vec<EnvironmentFact> {
        let themes = catalog::themes(self.dirs());
        let layauts = catalog::layauts(self.dirs());
        let addons = catalog::addons(self.dirs(), self.guard());
        vec![
            EnvironmentFact::new("themes")
                .with_note(BUILTIN_THEMES_NOTE)
                .with_items(themes.into_iter().map(|t| t.name)),
            EnvironmentFact::new("layauts")
                .with_note("The layaut decides which panels the desktop shows and where.")
                .with_items(layauts.into_iter().map(|l| l.name)),
            EnvironmentFact::new("addons")
                .with_note(BUILTIN_WIDGETS_NOTE)
                .with_items(addons.into_iter().map(|a| match &a.label {
                    Some(label) => format!("{} — {label} ({})", a.name, a.kind.as_str()),
                    None => format!("{} ({})", a.name, a.kind.as_str()),
                })),
        ]
    }

    fn effect(&self, call: &ToolCall) -> Effect {
        if !WRITING_TOOLS.contains(&call.name.as_str()) {
            return Effect::Read;
        }
        // Read straight from the call, and left generic when it is not
        // the shape the tool declared: this must describe the call as
        // it stands, and `invoke` is what tells the model its arguments
        // were wrong.
        let field = |name: &str| call.input.get(name).and_then(|v| v.as_str());
        let summary = match call.name.as_str() {
            TOOL_SET_THEME => match field("name") {
                Some("") => "clear the desktop theme, leaving the built-in one".to_string(),
                Some(name) => format!("set the desktop theme to {name}"),
                None => "set the desktop theme".to_string(),
            },
            TOOL_SET_LAYAUT => match field("name") {
                Some("") => "clear the desktop layaut, leaving the built-in one".to_string(),
                Some(name) => format!("set the desktop layaut to {name}"),
                None => "set the desktop layaut".to_string(),
            },
            _ => match (field("key"), call.input.get("value")) {
                (Some(key), Some(value)) => {
                    format!("set {key} to {} in the desktop configuration", plain(value))
                }
                (Some(key), None) => format!("change {key} in the desktop configuration"),
                _ => "change the desktop configuration".to_string(),
            },
        };
        let detail = match self.dirs().user_conf() {
            Ok(path) => format!(
                "edits {}; nacelle-desktop applies it the next time it starts",
                path.display()
            ),
            // Refusing comes later, in `invoke`. The user is still owed
            // an honest description of what is being proposed.
            Err(e) => e.to_string(),
        };
        Effect::Change(Change::new(summary).with_detail(detail))
    }

    fn invoke(&mut self, call: &ToolCall) -> ToolOutput {
        match self.run(&call.name, &call.input) {
            Ok(output) => ToolOutput::ok(output),
            Err(e) => ToolOutput::error(e.to_string()),
        }
    }
}

/// A JSON value as the user should read it in a one-line summary:
/// strings without their quotes, everything else as it is written.
fn plain(value: &serde_json::Value) -> String {
    match value.as_str() {
        Some(s) => s.to_string(),
        None => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The approval path is only as good as this list. A writing tool
    /// that answered [`Effect::Read`] would run without anyone being
    /// asked, so every tool whose description says it edits a file has
    /// to be in it.
    #[test]
    fn every_tool_that_edits_a_file_asks_first() {
        let tools = Toolbox::new(crate::tools::paths::DesktopDirs::new(None, None));
        for declaration in ToolRegistry::declarations(&tools) {
            let edits = declaration
                .description
                .contains("This edits a file on disk");
            let call = ToolCall {
                id: "id".into(),
                name: declaration.name.clone(),
                input: serde_json::json!({}),
            };
            let asks = matches!(tools.effect(&call), Effect::Change(_));
            assert_eq!(
                edits, asks,
                "{}: a tool that edits a file must ask, and one that does not must not",
                declaration.name
            );
        }
    }
}
