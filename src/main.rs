use std::backtrace::{Backtrace, BacktraceStatus};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;
use serenity::all::ShardManager;
use serenity::async_trait;
use serenity::futures::{StreamExt, TryStreamExt};
use serenity::http::{Http, StatusCode};
use serenity::model::gateway::Ready;
use serenity::model::prelude::{ChannelId, MessageId, MessagesIter};
use serenity::prelude::*;
use sqlx::{Connection, SqliteConnection};
use tokio::io::AsyncReadExt;
use tokio::task::JoinSet;
use twitter_syndication::TweetFetcher;

struct Handler;

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        eprintln!("{} is connected!", ready.user.name);

        // Read previously processed channels from database
        eprintln!("Reading DB");
        let mut conn = SqliteConnection::connect("database.sqlite").await.unwrap();
        let saved_channels = saved_channels(&mut conn).await;

        // Read new messages from all channels
        eprintln!("Reading channels");
        let mut set = JoinSet::new();
        for channel in ctx.data.read().await.get::<WatchedChannels>().unwrap() {
            let ctx = ctx.clone();
            let channel_id: ChannelId = channel.parse::<u64>().unwrap().into();
            let after: Option<MessageId> = saved_channels.get(&channel_id).copied();
            set.spawn(async move { read_channel_history(ctx, channel_id, after) });
        }

        // Begin transaction now that we are going to be writing
        begin_db_transaction(&mut conn).await;

        // Combine twitter users from all channels
        let mut twitter_users = BTreeSet::new();
        while let Some(res) = set.join_next().await {
            let (u, c) = res.unwrap().await;
            twitter_users.extend(u.into_iter());
            update_db_channels(&mut conn, c).await;
        }

        // Update database with new users and return all users
        let _all_twitter_users = update_db_users_and_return_all(&mut conn, twitter_users).await;
        commit_db_transaction(&mut conn).await;

        // Print users
        // eprintln!("Printing users");
        // for twitter_user in all_twitter_users {
        //     println!("{}", twitter_user)
        // }

        // Shutdown the bot cleanly
        ctx.data
            .read()
            .await
            .get::<ShardManagerContainer>()
            .unwrap()
            .shutdown_all()
            .await;
    }
}

#[tokio::main]
async fn main() {
    // exit the process on panic
    std::panic::set_hook(Box::new(|info| {
        let backtrace = Backtrace::capture();
        eprintln!("{info}");
        if matches!(backtrace.status(), BacktraceStatus::Captured) {
            eprintln!("{backtrace}");
        }
        std::process::exit(1);
    }));

    let config = read_config().await;

    let mut client = Client::builder(config.discord_token, GatewayIntents::empty())
        .event_handler(Handler)
        .await
        .expect("Err creating client");

    {
        let mut data = client.data.write().await;
        // Add Discord channels to scrape from config file
        data.insert::<WatchedChannels>(config.watched_channels.0);
        // Add shard manager so we can shutdown cleanly after completion
        data.insert::<ShardManagerContainer>(client.shard_manager.clone());
    }

    client.start().await.unwrap();
}

#[derive(Deserialize)]
struct Config {
    discord_token: String,
    watched_channels: WatchedChannels,
}

struct ShardManagerContainer;

impl TypeMapKey for ShardManagerContainer {
    type Value = Arc<ShardManager>;
}

#[derive(Deserialize)]
struct WatchedChannels(Vec<String>);

impl TypeMapKey for WatchedChannels {
    type Value = Vec<String>;
}

async fn read_config() -> Config {
    let file_name = std::env::args_os().nth(1).unwrap();
    let mut f = tokio::fs::File::open(file_name).await.unwrap();
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
        Self {
            channel,
            last_message: message,
        }
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
        Self {
            channel,
            last_message: message,
        }
    }
}

async fn read_channel_history(
    ctx: Context,
    channel: ChannelId,
    after: Option<MessageId>,
) -> (BTreeSet<String>, ChannelRow) {
    let mut twitter_users = BTreeSet::new();
    let mut last_processed_message = None;

    // Process all messages
    let messages = MessagesIter::<Http>::stream(&ctx, channel);
    tokio::pin!(messages);
    let mut i = 0;
    while let Some(message) = messages.next().await {
        let message = message.unwrap();
        if i % 1000 == 0 {
            eprintln!("heartbeat {}, {}", channel, message.id);
        }
        i += 1;

        // Stop if reached already processed message
        if let Some(after) = after {
            if message.id <= after {
                break;
            }
        }

        // Extract tweets
        let tweets = extract_tweets(&message.content);
        if !tweets.is_empty() {
            dbg!(&tweets);
        }
        twitter_users.extend(tweets.iter().map(|t| t.user.clone()));

        // Fetch tweet info and print for every tweet
        'tweets: for tweet in tweets {
            static TWEET_FETCHER: Lazy<TweetFetcher> = Lazy::new(|| TweetFetcher::new().unwrap());
            let mut retry_count = 0;
            let synd_tweet = loop {
                if retry_count >= 5 {
                    // It's probably actually not available
                    continue 'tweets;
                }
                match &TWEET_FETCHER.fetch(tweet.id).await {
                    Ok(t) => {
                        break t.clone();
                    }
                    outer @ Err(e) => {
                        if let Some(status) = e.status() {
                            if status == StatusCode::NOT_FOUND {
                                tokio::time::sleep(Duration::from_secs(5)).await;
                                continue;
                            } else if status.is_server_error() {
                                eprintln!(
                                    "error: tweet id {} status code: {}",
                                    tweet.id,
                                    status.as_u16()
                                );
                                continue 'tweets;
                            }
                            outer.as_ref().unwrap();
                        }
                    }
                }
                retry_count += 1;
            };
            println!("{}", serde_json::to_string(&synd_tweet).unwrap());
            eprintln!("{}", serde_json::to_string(&synd_tweet).unwrap());
        }

        last_processed_message = Some(
            last_processed_message
                .map(|m: MessageId| std::cmp::max(m, message.id))
                .unwrap_or(message.id),
        );
    }
    eprintln!("channel finished {}", channel);

    let channel_row = ChannelRow {
        channel,
        last_message: last_processed_message.unwrap_or(after.unwrap_or_default()),
    };

    (twitter_users, channel_row)
}

#[derive(Ord, PartialOrd, Eq, PartialEq, Debug)]
struct Tweet {
    user: String,
    id: u64,
}

fn extract_tweets(content: &str) -> BTreeSet<Tweet> {
    static RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"\b(twitter|x)\.com/(?P<user>.*?)/status/(?P<id>\d+)").unwrap());
    RE.captures(content)
        .into_iter()
        .map(|cap| {
            let user = cap.name("user").unwrap().as_str().to_lowercase();
            let id = cap.name("id").unwrap().as_str().parse().unwrap();
            Tweet { user, id }
        })
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

async fn update_db_channels(conn: &mut SqliteConnection, channel_row: ChannelRow) {
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

async fn update_db_users_and_return_all(
    conn: &mut SqliteConnection,
    mut twitter_users: BTreeSet<String>,
) -> BTreeSet<String> {
    // Update table
    for user in twitter_users.iter() {
        sqlx::query!(
            "INSERT OR IGNORE INTO twitter_users (user)
             VALUES ($1)",
            user
        )
        .execute(&mut *conn)
        .await
        .unwrap();
    }

    // Get all twitter users
    struct TwitterUsersRow {
        user: String,
    }
    let mut rows = sqlx::query_as!(TwitterUsersRow, "SELECT user FROM twitter_users").fetch(conn);
    while let Some(row) = rows.try_next().await.unwrap() {
        twitter_users.insert(row.user);
    }

    twitter_users
}

async fn begin_db_transaction(conn: &mut SqliteConnection) {
    sqlx::query!("BEGIN TRANSACTION")
        .execute(conn)
        .await
        .unwrap();
}

async fn commit_db_transaction(conn: &mut SqliteConnection) {
    sqlx::query!("COMMIT TRANSACTION")
        .execute(conn)
        .await
        .unwrap();
}
