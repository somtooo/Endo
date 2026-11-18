//! Zed's prompt as OpenCode takes it: a message, a command or a compaction,
//! with the files and skills it mentions.

use crate::client::PromptBody;
use crate::project_setup::ProjectSetup;
use acp_thread::MentionUri;
use agent_client_protocol::v2::schema as acp;
use collections::HashSet;
use serde_json::Value;
use std::borrow::Cow;
use std::fmt::Write as _;
use util::ResultExt as _;
use util::paths::PathStyle;

pub(crate) struct Prompt {
    pub kind: PromptKind,
    /// For a command, the text after its name.
    pub body: PromptBody,
}

impl Prompt {
    /// Decides whether `content` is a message, a command or a compaction. A
    /// skill typed as `/name` becomes a mention of it.
    pub(crate) fn parse(content: &[acp::ContentBlock], project_setup: &ProjectSetup) -> Self {
        let mut body = PromptBody::from_content(content);
        let message = |body| Self {
            kind: PromptKind::Message {
                display_metadata: encode_display_metadata(content),
            },
            body,
        };
        let Some(rest) = body.text.strip_prefix('/') else {
            return message(body);
        };
        let (name, arguments) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let kind = if name == "compact" {
            PromptKind::Compaction {
                follow_up: strip_command_name(content, name),
            }
        } else if project_setup.has_command(name) {
            PromptKind::Command {
                name: name.to_string(),
            }
        } else {
            // Zed accepts a skill typed as `/name` without picking it from
            // the menu. OpenCode has no such syntax, so it's sent as a
            // mention.
            if project_setup.has_skill(name) {
                if !body.skills.iter().any(|id| id == name) {
                    body.skills.insert(0, name.to_string());
                }
                body.text.replace_range(..1, "@");
            }
            return message(body);
        };
        body.text = arguments.trim().to_string();
        Self { kind, body }
    }
}

pub(crate) enum PromptKind {
    Message {
        /// The prompt as Zed shows it, saved with the message.
        display_metadata: Value,
    },
    Command {
        name: String,
    },
    Compaction {
        /// What was typed after `/compact`, which takes no argument, to send
        /// as the next message.
        follow_up: Option<Vec<acp::ContentBlock>>,
    },
}

/// Returns `None` when nothing but the command was typed.
fn strip_command_name(content: &[acp::ContentBlock], name: &str) -> Option<Vec<acp::ContentBlock>> {
    let mut rest = content.to_vec();
    let position = rest
        .iter()
        .position(|block| matches!(block, acp::ContentBlock::Text(_)))?;
    if let Some(acp::ContentBlock::Text(block)) = rest.get_mut(position) {
        let text = block.text.trim_start();
        let text = text.strip_prefix('/').unwrap_or(text);
        block.text = text
            .strip_prefix(name)
            .unwrap_or(text)
            .trim_start()
            .to_string();
        if block.text.is_empty() {
            rest.remove(position);
        }
    }
    let typed_more = rest.iter().any(|block| match block {
        acp::ContentBlock::Text(block) => !block.text.trim().is_empty(),
        _ => true,
    });
    typed_more.then_some(rest)
}

/// A block that mentions a file, a skill or other context.
struct Mention<'a> {
    uri: &'a str,
    name: Cow<'a, str>,
    /// The context Zed already read, if any.
    text: Option<&'a str>,
}

impl PromptBody {
    /// Skills go as their ID; whole files and images go as attachments the
    /// server reads itself; selections, symbols and other resolved context
    /// go inline.
    fn from_content(content: &[acp::ContentBlock]) -> Self {
        let mut body = PromptBody {
            text: String::new(),
            files: Vec::new(),
            skills: Vec::new(),
        };
        for block in content {
            let mention = match block {
                acp::ContentBlock::Text(content) => {
                    body.text.push_str(&content.text);
                    continue;
                }
                acp::ContentBlock::ResourceLink(link) => Mention {
                    uri: &link.uri,
                    name: Cow::Borrowed(&link.name),
                    text: None,
                },
                acp::ContentBlock::Resource(resource) => match &resource.resource {
                    acp::EmbeddedResourceResource::TextResourceContents(contents) => Mention {
                        uri: &contents.uri,
                        // The URI escapes characters such as spaces; the
                        // name the model reads shouldn't.
                        name: MentionUri::parse(&contents.uri, PathStyle::local())
                            .map(|uri| Cow::Owned(uri.name()))
                            .unwrap_or(Cow::Borrowed(&contents.uri)),
                        text: Some(&contents.text),
                    },
                    _ => {
                        log::warn!("OpenCode cannot send an embedded binary resource");
                        continue;
                    }
                },
                acp::ContentBlock::Image(image) => {
                    body.files.push(serde_json::json!({
                        "uri": format!("data:{};base64,{}", image.mime_type.0, image.data),
                        "name": "image",
                    }));
                    continue;
                }
                _ => continue,
            };
            body.add_mention(mention);
        }
        let mut seen = HashSet::default();
        body.skills.retain(|id| seen.insert(id.clone()));
        body
    }

    fn add_mention(&mut self, mention: Mention) {
        if let Some(id) = parse_skill_id(mention.uri) {
            write!(self.text, "@{id}").log_err();
            self.skills.push(id);
        } else if matches!(
            MentionUri::parse(mention.uri, PathStyle::local()),
            Ok(MentionUri::File { .. })
        ) {
            self.files
                .push(serde_json::json!({ "uri": mention.uri, "name": mention.name }));
            write!(self.text, "@{}", mention.name).log_err();
        } else if let Some(text) = mention.text {
            write!(
                self.text,
                "\n<context uri=\"{}\">\n{}\n</context>\n",
                mention.uri, text
            )
            .log_err();
        } else {
            write!(self.text, "@{}", mention.name).log_err();
        }
    }
}

fn parse_skill_id(uri: &str) -> Option<String> {
    match MentionUri::parse(uri, PathStyle::local()).ok()? {
        MentionUri::Skill { name, .. } => Some(name),
        _ => None,
    }
}

const DISPLAY_METADATA_KEY: &str = "zed";

/// The prompt as Zed showed it, saved in the message's metadata. Images are
/// left out because the message's files already carry them. Skills keep only
/// their link: OpenCode loads a skill itself, so its file isn't needed again.
pub(crate) fn encode_display_metadata(content: &[acp::ContentBlock]) -> Value {
    let blocks: Vec<acp::ContentBlock> = content
        .iter()
        .filter_map(|block| match block {
            acp::ContentBlock::Image(_) => None,
            acp::ContentBlock::Resource(resource) => {
                if let acp::EmbeddedResourceResource::TextResourceContents(contents) =
                    &resource.resource
                    && let Ok(uri @ MentionUri::Skill { .. }) =
                        MentionUri::parse(&contents.uri, PathStyle::local())
                {
                    return Some(acp::ContentBlock::ResourceLink(acp::ResourceLink::new(
                        uri.name(),
                        contents.uri.clone(),
                    )));
                }
                Some(block.clone())
            }
            _ => Some(block.clone()),
        })
        .collect();
    serde_json::json!({ DISPLAY_METADATA_KEY: { "content": blocks } })
}

pub(crate) fn decode_display_metadata(metadata: Option<&Value>) -> Option<Vec<acp::ContentBlock>> {
    let content = metadata?.get(DISPLAY_METADATA_KEY)?.get("content")?;
    serde_json::from_value(content.clone()).log_err()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Command, Skill};
    use std::path::PathBuf;

    fn skill_resource(name: &str) -> acp::ContentBlock {
        let uri = MentionUri::Skill {
            name: name.to_string(),
            source: String::new(),
            skill_file_path: PathBuf::from(format!("/skills/{name}/SKILL.md")),
        };
        acp::ContentBlock::Resource(acp::EmbeddedResource::new(
            acp::EmbeddedResourceResource::TextResourceContents(acp::TextResourceContents::new(
                "skill file content",
                uri.to_uri().to_string(),
            )),
        ))
    }

    #[test]
    fn file_mentions_use_the_unescaped_name() {
        let uri = MentionUri::File {
            abs_path: PathBuf::from("/notes/AI ML overview.md"),
        }
        .to_uri()
        .to_string();
        let body = PromptBody::from_content(&[
            acp::ContentBlock::Resource(acp::EmbeddedResource::new(
                acp::EmbeddedResourceResource::TextResourceContents(
                    acp::TextResourceContents::new("file content", uri.clone()),
                ),
            )),
            acp::ContentBlock::Text(acp::TextContent::new(" is this it")),
        ]);
        assert_eq!(body.text, "@AI ML overview.md is this it");
        assert_eq!(
            body.files,
            vec![serde_json::json!({ "uri": uri, "name": "AI ML overview.md" })]
        );
    }

    #[test]
    fn other_mentions_stay_in_the_text() {
        let body = PromptBody::from_content(&[
            acp::ContentBlock::Resource(acp::EmbeddedResource::new(
                acp::EmbeddedResourceResource::TextResourceContents(
                    acp::TextResourceContents::new("let answer = 42;", "file:///a.rs#L1:1"),
                ),
            )),
            acp::ContentBlock::ResourceLink(acp::ResourceLink::new("rules", "zed:///rules/1")),
        ]);
        assert_eq!(
            body.text,
            "\n<context uri=\"file:///a.rs#L1:1\">\nlet answer = 42;\n</context>\n@rules"
        );
        assert!(body.files.is_empty());
    }

    #[test]
    fn directory_mentions_are_not_file_attachments() {
        let body = PromptBody::from_content(&[acp::ContentBlock::Resource(
            acp::EmbeddedResource::new(acp::EmbeddedResourceResource::TextResourceContents(
                acp::TextResourceContents::new("directory listing", "file:///project/src/"),
            )),
        )]);
        assert!(body.files.is_empty());
        assert!(body.text.contains("directory listing"));
    }

    #[test]
    fn skill_mentions_are_sent_by_id() {
        let body = PromptBody::from_content(&[
            skill_resource("deep-research"),
            acp::ContentBlock::Text(acp::TextContent::new(" look into this ")),
            skill_resource("deep-research"),
        ]);
        assert_eq!(body.text, "@deep-research look into this @deep-research");
        assert_eq!(body.skills, vec!["deep-research".to_string()]);
        assert!(body.files.is_empty());
    }

    fn project_setup() -> ProjectSetup {
        ProjectSetup {
            commands: vec![Command {
                name: "review".into(),
                description: None,
            }],
            skills: vec![Skill {
                id: "deep-research".into(),
                description: String::new(),
                path: PathBuf::from("/skills/deep-research/SKILL.md"),
                content: String::new(),
                scope: String::new(),
            }],
            ..ProjectSetup::default()
        }
    }

    fn text(text: &str) -> Vec<acp::ContentBlock> {
        vec![acp::ContentBlock::Text(acp::TextContent::new(text))]
    }

    #[test]
    fn a_slash_command_runs() {
        let command = Prompt::parse(&text("/review  the diff "), &project_setup());
        assert!(matches!(command.kind, PromptKind::Command { name } if name == "review"));
        assert_eq!(command.body.text, "the diff");

        assert!(matches!(
            Prompt::parse(&text("/compact  "), &project_setup()).kind,
            PromptKind::Compaction { follow_up: None }
        ));
    }

    #[test]
    fn a_slash_skill_becomes_a_mention() {
        let skill = Prompt::parse(&text("/deep-research this"), &project_setup());
        assert!(matches!(skill.kind, PromptKind::Message { .. }));
        assert_eq!(skill.body.text, "@deep-research this");
        assert_eq!(skill.body.skills, vec!["deep-research".to_string()]);

        let unknown = Prompt::parse(&text("/nothing here"), &project_setup());
        assert!(matches!(unknown.kind, PromptKind::Message { .. }));
        assert_eq!(unknown.body.text, "/nothing here");
    }

    #[test]
    fn text_after_compact_becomes_the_next_message() {
        let mut content = text("/compact keep the API notes");
        content.push(acp::ContentBlock::ResourceLink(acp::ResourceLink::new(
            "a.rs",
            "file:///project/a.rs",
        )));
        let PromptKind::Compaction {
            follow_up: Some(follow_up),
        } = Prompt::parse(&content, &project_setup()).kind
        else {
            panic!("expected a compaction with a follow-up");
        };
        assert_eq!(follow_up.len(), 2);
        assert!(
            matches!(&follow_up[0], acp::ContentBlock::Text(block) if block.text == "keep the API notes")
        );
    }
}
