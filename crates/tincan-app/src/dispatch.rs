//! `App::call`: deserialize the camelCase args of one `types.ts` command, run it, serialize
//! the result.

use std::sync::Arc;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::commands::{parse_channel, parse_conv, parse_user};
use crate::app::Core;
use crate::dto;
use crate::media;
use crate::transfers::parse_hash;

/// Commands handled outside the media layer.
pub(crate) const CORE_COMMANDS: &[&str] = &[
    "app_snapshot",
    "set_name",
    "set_avatar",
    "set_server_icon",
    "create_server",
    "create_invite",
    "get_invite_code",
    "rotate_invite_code",
    "get_friend_code",
    "rotate_friend_code",
    "join_server",
    "leave_server",
    "rename_server",
    "channels",
    "create_channel",
    "rename_channel",
    "delete_channel",
    "members",
    "messages",
    "send_message",
    "delete_message",
    "mark_read",
    "typing",
    "transfer",
    "download",
    "open_file",
    "file_url",
    "open_dm",
    "dm_watch",
    "add_contact",
    "settings_set",
];

/// Deserialize `args` (null counts as `{}`); the error names the command.
pub(crate) fn parse_args<T: DeserializeOwned>(cmd: &str, args: Value) -> Result<T, String> {
    let args = if args.is_null() { Value::Object(serde_json::Map::new()) } else { args };
    serde_json::from_value(args).map_err(|e| format!("Invalid arguments for {cmd}: {e}"))
}

fn ok<T: Serialize>(v: T) -> Result<Value, String> {
    serde_json::to_value(v).map_err(|e| {
        tracing::error!(error = %e, "encoding a command result failed");
        "Something went wrong. Try again.".to_string()
    })
}

mod args {
    use serde::Deserialize;

    use crate::dto;

    #[derive(Deserialize)]
    pub struct Name {
        pub name: String,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Server {
        pub server_id: String,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Watch {
        #[serde(default)]
        pub conv_id: Option<String>,
    }

    #[derive(Deserialize)]
    pub struct Join {
        pub invite: String,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ServerName {
        pub server_id: String,
        pub name: String,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct CreateChannel {
        pub server_id: String,
        pub name: String,
        pub kind: dto::ChannelKind,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct RenameChannel {
        pub server_id: String,
        pub channel_id: String,
        pub name: String,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ServerChannel {
        pub server_id: String,
        pub channel_id: String,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Messages {
        pub conv_id: String,
        #[serde(default)]
        pub channel_id: Option<String>,
        #[serde(default)]
        pub before: Option<String>,
        pub limit: u32,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Send {
        pub conv_id: String,
        #[serde(default)]
        pub channel_id: Option<String>,
        pub text: String,
        #[serde(default)]
        pub attachments: Vec<String>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct DeleteMessage {
        pub conv_id: String,
        pub message_id: String,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ConvChannel {
        pub conv_id: String,
        #[serde(default)]
        pub channel_id: Option<String>,
    }

    #[derive(Deserialize)]
    pub struct Hash {
        pub hash: String,
    }

    #[derive(Deserialize)]
    pub struct HashName {
        pub hash: String,
        #[serde(default)]
        pub name: String,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct User {
        pub user_id: String,
    }

    #[derive(Deserialize)]
    pub struct Code {
        pub code: String,
    }

    #[derive(Deserialize)]
    pub struct Image {
        #[serde(default)]
        pub bytes: Option<Vec<u8>>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ServerImage {
        pub server_id: String,
        #[serde(default)]
        pub bytes: Option<Vec<u8>>,
    }

    #[derive(Deserialize)]
    pub struct SettingsSet {
        pub settings: dto::Settings,
    }
}

/// Run one command.
pub(crate) async fn call(core: &Arc<Core>, cmd: &str, a: Value) -> Result<Value, String> {
    if core.is_closed() {
        return Err("Tincan is shutting down.".into());
    }
    match cmd {
        "app_snapshot" => {
            core.flush_notices();
            ok(core.snapshot())
        }
        "set_name" => {
            let a: args::Name = parse_args(cmd, a)?;
            ok(core.set_name(&a.name)?)
        }
        "set_avatar" => {
            let a: args::Image = parse_args(cmd, a)?;
            ok(core.cmd_set_avatar(a.bytes).await?)
        }
        "set_server_icon" => {
            let a: args::ServerImage = parse_args(cmd, a)?;
            ok(core.cmd_set_server_icon(parse_conv(&a.server_id)?, a.bytes).await?)
        }
        "create_server" => {
            let a: args::Name = parse_args(cmd, a)?;
            ok(core.cmd_create_server(&a.name).await?)
        }
        "create_invite" => {
            let a: args::Server = parse_args(cmd, a)?;
            ok(core.cmd_create_invite(parse_conv(&a.server_id)?).await?)
        }
        "get_invite_code" => {
            let a: args::Server = parse_args(cmd, a)?;
            ok(core.cmd_get_invite_code(parse_conv(&a.server_id)?).await?)
        }
        "rotate_invite_code" => {
            let a: args::Server = parse_args(cmd, a)?;
            ok(core.cmd_rotate_invite_code(parse_conv(&a.server_id)?).await?)
        }
        "get_friend_code" => ok(core.cmd_get_friend_code().await?),
        "rotate_friend_code" => ok(core.cmd_rotate_friend_code().await?),
        "join_server" => {
            let a: args::Join = parse_args(cmd, a)?;
            ok(core.cmd_join_server(&a.invite).await?)
        }
        "leave_server" => {
            let a: args::Server = parse_args(cmd, a)?;
            core.cmd_leave_server(parse_conv(&a.server_id)?).await?;
            Ok(Value::Null)
        }
        "rename_server" => {
            let a: args::ServerName = parse_args(cmd, a)?;
            core.cmd_rename_server(parse_conv(&a.server_id)?, &a.name).await?;
            Ok(Value::Null)
        }
        "channels" => {
            let a: args::Server = parse_args(cmd, a)?;
            ok(core.cmd_channels(parse_conv(&a.server_id)?)?)
        }
        "create_channel" => {
            let a: args::CreateChannel = parse_args(cmd, a)?;
            ok(core.cmd_create_channel(parse_conv(&a.server_id)?, &a.name, a.kind).await?)
        }
        "rename_channel" => {
            let a: args::RenameChannel = parse_args(cmd, a)?;
            core.cmd_rename_channel(parse_conv(&a.server_id)?, parse_channel(&a.channel_id)?, &a.name).await?;
            Ok(Value::Null)
        }
        "delete_channel" => {
            let a: args::ServerChannel = parse_args(cmd, a)?;
            core.cmd_delete_channel(parse_conv(&a.server_id)?, parse_channel(&a.channel_id)?).await?;
            Ok(Value::Null)
        }
        "members" => {
            let a: args::Server = parse_args(cmd, a)?;
            ok(core.cmd_members(parse_conv(&a.server_id)?)?)
        }
        "messages" => {
            let a: args::Messages = parse_args(cmd, a)?;
            ok(core.cmd_messages(parse_conv(&a.conv_id)?, a.channel_id.as_deref(), a.before.as_deref(), a.limit)?)
        }
        "send_message" => {
            let a: args::Send = parse_args(cmd, a)?;
            ok(core.cmd_send_message(parse_conv(&a.conv_id)?, a.channel_id.as_deref(), &a.text, &a.attachments).await?)
        }
        "delete_message" => {
            let a: args::DeleteMessage = parse_args(cmd, a)?;
            let id = tincan_core::EventId::from_hex(&a.message_id).ok_or_else(|| "That message id is not valid.".to_string())?;
            core.cmd_delete_message(parse_conv(&a.conv_id)?, id).await?;
            Ok(Value::Null)
        }
        "mark_read" => {
            let a: args::ConvChannel = parse_args(cmd, a)?;
            core.cmd_mark_read(parse_conv(&a.conv_id)?, a.channel_id.as_deref())?;
            Ok(Value::Null)
        }
        "typing" => {
            let a: args::ConvChannel = parse_args(cmd, a)?;
            core.cmd_typing(parse_conv(&a.conv_id)?, a.channel_id.as_deref()).await?;
            Ok(Value::Null)
        }
        "transfer" => {
            let a: args::Hash = parse_args(cmd, a)?;
            ok(core.cmd_transfer(parse_hash(&a.hash)?))
        }
        "download" => {
            let a: args::Hash = parse_args(cmd, a)?;
            ok(core.cmd_download(parse_hash(&a.hash)?).await?)
        }
        "open_file" => {
            let a: args::HashName = parse_args(cmd, a)?;
            core.cmd_open_file(parse_hash(&a.hash)?, &a.name).await?;
            Ok(Value::Null)
        }
        "file_url" => {
            let a: args::HashName = parse_args(cmd, a)?;
            ok(core.cmd_file_url(parse_hash(&a.hash)?))
        }
        "open_dm" => {
            let a: args::User = parse_args(cmd, a)?;
            ok(core.cmd_open_dm(parse_user(&a.user_id)?).await?)
        }
        "dm_watch" => {
            let a: args::Watch = parse_args(cmd, a)?;
            core.cmd_dm_watch(a.conv_id.as_deref().map(parse_conv).transpose()?)?;
            Ok(Value::Null)
        }
        "add_contact" => {
            let a: args::Code = parse_args(cmd, a)?;
            ok(core.cmd_add_contact(&a.code).await?)
        }
        "settings_set" => {
            let a: args::SettingsSet = parse_args(cmd, a)?;
            ok::<dto::Settings>(core.cmd_settings_set(a.settings)?)
        }
        _ if media::COMMANDS.contains(&cmd) => core.media.call(core, cmd, a).await,
        _ => Err(format!("Unknown command: {cmd}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_lists_do_not_overlap() {
        for c in CORE_COMMANDS {
            assert!(!media::COMMANDS.contains(c), "{c} is in both lists");
        }
    }

    #[test]
    fn null_args_are_an_empty_object() {
        #[derive(serde::Deserialize)]
        struct NoArgs {}
        assert!(parse_args::<NoArgs>("x", Value::Null).is_ok());
        let e = parse_args::<args::Name>("set_name", Value::Null).err().unwrap();
        assert!(e.starts_with("Invalid arguments for set_name"));
    }
}
