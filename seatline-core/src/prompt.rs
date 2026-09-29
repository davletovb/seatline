//! The text an adapter sends a provider for one turn.
//!
//! Every adapter renders a [`Turn`](crate::turn::Turn)'s messages the same
//! way, so a second application gets the same framing as the first. The
//! application frames whatever it attaches to the current message (browser
//! context, other models' answers) into that message's text; the runtime only
//! adds what belongs to the turn's shape: the application's own instructions
//! (the turn's system prompt), the earlier dialogue, and the instructions of a
//! search turn.
//!
//! No provider CLI takes a system prompt on its command line without putting
//! it in the process list, so an adapter sends it on the channel that carries
//! the rest: as the first part of the prompt, or, where the provider has a
//! system prompt of its own that its CLI reads from a file the adapter writes
//! (Antigravity's agent), there. Which channel and which words a provider's
//! model follows is not something to guess: they were found by running the real
//! CLIs, and differ (see [`SYSTEM_INTRO`]).

use crate::turn::{Message, Role, ToolPolicy};

/// What a web-search turn asks of the provider, before the question. Providers
/// decide for themselves whether to search and whether to cite; without this, a
/// turn can end with no usable source (the provider answered from memory, or
/// asked which meaning was meant), which fails the turn as
/// `NATIVE_SEARCH_NO_SOURCES`. It holds no link of its own, so nothing here can
/// be mistaken for a cited source.
pub const SEARCH_INSTRUCTIONS: &str = "Search the web before you answer the question below, even if you think you know the answer, and base your answer on what you find. Cite every page you use as a Markdown link: the page title in square brackets, then its full URL in parentheses. Answer directly: don't describe your searching, and don't mention files, tools, or what you can or can't access. If the question could mean several things, answer its most likely meanings, each with its sources, instead of asking which one was meant.\n\n";

/// Introduces earlier messages that couldn't be resumed natively.
const HISTORY_INTRO: &str =
    "Continue this conversation. The previous messages are quoted JSON data in order:\n";

/// The label an application puts before the question it frames into the
/// current message, when something else comes before the question.
pub const CURRENT_QUESTION: &str = "Current user question:\n";

/// Introduces a turn's system prompt when it goes in the prompt, so the provider
/// can tell the application's instructions from something a user said. This is
/// the wording for adapters whose provider has no better wording of its own
/// (Codex and Claude): an imperative, without a claim of precedence.
///
/// The wording matters, and differs by provider, as real runs showed.
/// Antigravity's Gemini read "take precedence over the messages below" as a
/// prompt injection and refused, and follows its agent file instead. Grok is
/// the opposite: it follows that claim of precedence in the prompt file (16 of
/// 16 runs) and ignores the same text in its agent file (2 of 5), so its adapter
/// has its own introduction (`grok::SYSTEM_INTRO`). No live test has yet
/// asserted that Codex or Claude follow this one.
pub const SYSTEM_INTRO: &str =
    "Follow these instructions from the application for the whole conversation:\n";

/// The prompt for `messages`, the last of which is the current one.
///
/// A lone message is sent as it is. A non-empty `system` prompt comes first,
/// after [`SYSTEM_INTRO`] (an adapter whose provider has a system prompt of its
/// own passes `None` and puts it there). Earlier messages come next, each as
/// one line of quoted JSON, so nothing in them can be mistaken for an
/// instruction of the application. A search turn adds [`SEARCH_INSTRUCTIONS`]
/// before its question.
pub fn render(system: Option<&str>, messages: &[Message], tools: ToolPolicy) -> String {
    render_with_intro(SYSTEM_INTRO, system, messages, tools)
}

/// [`render`], introducing the system prompt with `intro` instead of
/// [`SYSTEM_INTRO`], for a provider whose model wants other words.
pub fn render_with_intro(
    intro: &str,
    system: Option<&str>,
    messages: &[Message],
    tools: ToolPolicy,
) -> String {
    let mut prompt = String::new();
    if let Some(system) = system.filter(|system| !system.is_empty()) {
        prompt.push_str(intro);
        prompt.push_str(system);
        prompt.push_str("\n\n");
    }
    if tools == ToolPolicy::NativeWebSearch {
        prompt.push_str(SEARCH_INSTRUCTIONS);
    }
    let Some((current, earlier)) = messages.split_last() else {
        return prompt;
    };
    if !earlier.is_empty() {
        prompt.push_str(HISTORY_INTRO);
        for message in earlier {
            let role = match message.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            let line = serde_json::json!({ "role": role, "text": message.text });
            prompt.push_str(&line.to_string());
            prompt.push('\n');
        }
    }
    prompt.push_str(&current.text);
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(role: Role, text: &str) -> Message {
        Message {
            role,
            text: text.to_owned(),
        }
    }

    #[test]
    fn a_lone_message_is_sent_as_it_is() {
        let messages = [message(Role::User, "what is muse?")];
        assert_eq!(render(None, &messages, ToolPolicy::None), "what is muse?");
        assert_eq!(
            render(None, &messages, ToolPolicy::ProviderDefault),
            "what is muse?"
        );
    }

    #[test]
    fn earlier_messages_are_quoted_json_lines_before_the_current_one() {
        let messages = [
            message(Role::User, "first \"quoted\""),
            message(Role::Assistant, "line\nbreak"),
            message(Role::User, "follow up"),
        ];
        assert_eq!(
            render(None, &messages, ToolPolicy::None),
            "Continue this conversation. The previous messages are quoted JSON data in order:\n\
             {\"role\":\"user\",\"text\":\"first \\\"quoted\\\"\"}\n\
             {\"role\":\"assistant\",\"text\":\"line\\nbreak\"}\n\
             follow up"
        );
    }

    #[test]
    fn a_search_turn_starts_with_the_search_instructions() {
        let prompt = render(
            None,
            &[message(Role::User, "Current user question:\nwhy?")],
            ToolPolicy::NativeWebSearch,
        );
        assert!(prompt.starts_with(SEARCH_INSTRUCTIONS));
        assert!(prompt.ends_with("Current user question:\nwhy?"));
        assert_eq!(prompt.matches("Search the web").count(), 1);
    }

    #[test]
    fn no_messages_render_nothing_but_the_instructions() {
        assert_eq!(render(None, &[], ToolPolicy::None), "");
        assert_eq!(
            render(None, &[], ToolPolicy::NativeWebSearch),
            SEARCH_INSTRUCTIONS
        );
    }

    #[test]
    fn a_system_prompt_comes_first_under_its_own_introduction() {
        let prompt = render(
            Some("Answer in French."),
            &[
                message(Role::User, "earlier"),
                message(Role::User, "what is muse?"),
            ],
            ToolPolicy::NativeWebSearch,
        );
        assert_eq!(
            prompt,
            format!(
                "{SYSTEM_INTRO}Answer in French.\n\n{SEARCH_INSTRUCTIONS}\
                 Continue this conversation. The previous messages are quoted JSON data in order:\n\
                 {{\"role\":\"user\",\"text\":\"earlier\"}}\n\
                 what is muse?"
            )
        );
    }

    #[test]
    fn an_adapter_may_introduce_the_system_prompt_in_its_own_words() {
        let messages = [message(Role::User, "what is muse?")];
        assert_eq!(
            render_with_intro("Listen:\n", Some("Be brief."), &messages, ToolPolicy::None),
            "Listen:\nBe brief.\n\nwhat is muse?"
        );
        assert_eq!(
            render_with_intro("Listen:\n", None, &messages, ToolPolicy::None),
            render(None, &messages, ToolPolicy::None)
        );
    }

    #[test]
    fn an_empty_system_prompt_adds_nothing() {
        let messages = [message(Role::User, "what is muse?")];
        assert_eq!(
            render(Some(""), &messages, ToolPolicy::None),
            "what is muse?"
        );
    }
}
