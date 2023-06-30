use std::collections::{HashMap, HashSet};

use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;
use serenity::async_trait;
use serenity::futures::{StreamExt, TryStreamExt};
use serenity::http::Http;
use serenity::model::gateway::Ready;
use serenity::model::prelude::{ChannelId, MessageId, MessagesIter};
use serenity::prelude::*;
use sqlx::{Connection, SqliteConnection};
use tokio::io::AsyncReadExt;
use tokio::task::JoinSet;

struct Handler;

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        eprintln!("{} is connected!", ready.user.name);

        // Read previously processed channels from database
        let mut conn = SqliteConnection::connect("database.sqlite").await.unwrap();
        let saved_channels = saved_channels(&mut conn).await;

        // Read new messages from all channels
        let mut set = JoinSet::new();
        for channel in ctx.data.read().await.get::<WatchedChannels>().unwrap() {
            let ctx = ctx.clone();
            let channel_id: ChannelId = channel.parse::<u64>().unwrap().into();
            let after: Option<MessageId> = saved_channels.get(&channel_id).copied();
            set.spawn(async move { read_channel_history(ctx, channel_id, after) });
        }

        // Combine twitter users from all channels
        let mut twitter_users = HashSet::new();
        while let Some(res) = set.join_next().await {
            let (u, c) = res.unwrap().await;
            twitter_users.extend(u.into_iter());
            update_db(&mut conn, c).await;
        }

        for twitter_user in twitter_users {
            println!("{}", twitter_user)
        }
    }
}

#[tokio::main]
async fn main() {
    let config = read_config().await;

    let mut client = Client::builder(config.discord_token, GatewayIntents::empty())
        .event_handler(Handler)
        .await
        .expect("Err creating client");

    {
        let mut data = client.data.write().await;
        data.insert::<WatchedChannels>(config.watched_channels.0);
    }

    if let Err(why) = client.start().await {
        eprintln!("Client error: {:?}", why);
    }
}

#[derive(Deserialize)]
struct Config {
    discord_token: Box<str>,
    watched_channels: WatchedChannels,
}

#[derive(Deserialize)]
struct WatchedChannels(Vec<Box<str>>);

impl TypeMapKey for WatchedChannels {
    type Value = Vec<Box<str>>;
}

async fn read_config() -> Config {
    let mut f = tokio::fs::File::open("config.toml").await.unwrap();
    let mut config_str = String::new();
    f.read_to_string(&mut config_str).await.unwrap();
    toml::from_str(&config_str).unwrap()
}

struct ChannelRow {
    channel: ChannelId,
    last_message: MessageId,
}

impl From<SqlChannelRow> for ChannelRow {
    fn from(value: SqlChannelRow) -> Self {
        let channel: ChannelId = value.channel.parse::<u64>().unwrap().into();
        let message: MessageId = value.last_message.parse::<u64>().unwrap().into();
        Self { channel, last_message: message }
    }
}

struct SqlChannelRow {
    channel: String,
    last_message: String,
}

impl From<ChannelRow> for SqlChannelRow {
    fn from(value: ChannelRow) -> Self {
        let channel = value.channel.to_string();
        let message = value.last_message.to_string();
        Self { channel, last_message: message }
    }
}

async fn read_channel_history(
    ctx: Context,
    channel: ChannelId,
    after: Option<MessageId>,
) -> (HashSet<Box<str>>, ChannelRow) {
    let mut twitter_users = HashSet::new();
    let mut last_processed_message = None;

    // Process all messages
    let messages = MessagesIter::<Http>::stream(&ctx, channel);
    tokio::pin!(messages);
    let mut count = 0;
    while let Some(message) = messages.next().await {
        let message = message.unwrap();

        count += 1;
        if count > 200 {
            break;
        }

        // Stop if reached already processed message
        if let Some(after) = after {
            if message.id.0 <= after.0 {
                break;
            }
        }

        // Extract Twitter users
        twitter_users.extend(extract_twitter_users(&message.content).into_iter());
        last_processed_message = Some(message.id);
    }

    let channel_row = ChannelRow {
        channel,
        last_message: last_processed_message.unwrap_or(after.unwrap_or(MessageId(0))),
    };

    (twitter_users, channel_row)
}

fn extract_twitter_users(content: &str) -> HashSet<Box<str>> {
    static RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"twitter\.com/(?P<user>.*?)/status/\d+").unwrap());
    RE.captures(content)
        .into_iter()
        .map(|cap| cap.name("user").unwrap().as_str().into())
        .collect()
}

async fn saved_channels(conn: &mut SqliteConnection) -> HashMap<ChannelId, MessageId> {
    let mut saved_channels = HashMap::new();

    let mut rows = sqlx::query_as!(SqlChannelRow, "SELECT * FROM channels").fetch(conn);
    while let Some(row) = rows.try_next().await.unwrap() {
        let row: ChannelRow = row.into();
        saved_channels.insert(row.channel, row.last_message);
    }

    saved_channels
}

async fn update_db(conn: &mut SqliteConnection, channel_row: ChannelRow) {
    let channel = channel_row.channel.to_string();
    let message = channel_row.last_message.to_string();
    sqlx::query!(
        "INSERT INTO channels (channel, last_message)
                 VALUES ($1, $2)
                 ON CONFLICT(channel) DO UPDATE SET last_message=$2",
        channel,
        message,
    )
    .execute(conn)
    .await
    .unwrap();
}
