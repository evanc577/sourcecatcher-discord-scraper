use core::panic;
use std::backtrace::{Backtrace, BacktraceStatus};
use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;
use serenity::all::ShardManager;
use serenity::async_trait;
use serenity::futures::{StreamExt, TryStreamExt};
use serenity::http::Http;
use serenity::model::gateway::Ready;
use serenity::model::prelude::{ChannelId, MessageId, MessagesIter};
use serenity::prelude::*;
use sqlx::{Connection, SqliteConnection};
use tokio::io::AsyncReadExt;
use tokio::task::JoinSet;
use twitter_syndication::tweet::TweetType as SyndicationTweetType;
use twitter_syndication::TweetFetcher;

struct Handler;

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        static STARTED: AtomicBool = AtomicBool::new(false);
        match STARTED.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => {
                eprintln!("{} is connected!", ready.user.name);
            }
            Err(_) => {
                eprintln!("Received another connect");
                panic!();
            }
        }

        // Read previously processed channels from database
        eprintln!("Reading DB");
        let db_file = ctx.data.read().await.get::<DbFile>().unwrap().to_owned();
        let mut conn = SqliteConnection::connect(&db_file).await.unwrap();
        let saved_channels = saved_channels(&mut conn).await;

        // Read new messages from all channels
        eprintln!("Spawning channel readers");
        let mut set = JoinSet::new();
        for channel in ctx.data.read().await.get::<WatchedChannels>().unwrap() {
            let ctx = ctx.clone();
            let channel_id: ChannelId = channel.parse::<u64>().unwrap().into();
            let after: Option<MessageId> = saved_channels.get(&channel_id).copied();
            set.spawn(async move { read_channel_history(ctx, channel_id, after).await });
        }

        eprintln!("Reading channels");

        // Begin transaction now that we are going to be writing
        begin_db_transaction(&mut conn).await;

        // Combine twitter users from all channels
        let mut twitter_users = BTreeSet::new();
        while let Some(res) = set.join_next().await {
            let (u, c) = res.unwrap();
            twitter_users.extend(u.into_iter());
            update_db_channels(&mut conn, c).await;
        }

        // Update database with new users and return all users
        let _all_twitter_users = update_db_users_and_return_all(&mut conn, twitter_users).await;
        commit_db_transaction(&mut conn).await;

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
    console_subscriber::init();

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
        // Add database file from config file
        data.insert::<DbFile>(config.database_file);
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
    database_file: String,
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

#[derive(Deserialize)]
struct DbFile;

impl TypeMapKey for DbFile {
    type Value = String;
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
    let channel_name = channel.name(&ctx).await.unwrap();
    let channel_text = format!("{channel} ({channel_name})");
    eprintln!("read_channel_history() channel: {channel_text} after: {after:?}");

    let mut twitter_users = BTreeSet::new();
    let mut last_processed_message = None;

    // Twitter will sometimes return 404 for a specific tweet when rate limited, keep track of all
    // 404ed tweets and retry them later after a few minutes
    let mut retry_tweets = Vec::new();

    // Helper function to process twitter responses
    let mut do_fetch_tweet = |tweet: Tweet, fetch_tweet_result, retry_tweets: &mut Vec<Tweet>| {
        match fetch_tweet_result {
            FetchTweetResult::Ok(t) => {
                println!("{}", serde_json::to_string(&t).unwrap());
                eprintln!("{}", serde_json::to_string(&t).unwrap());
                twitter_users.insert(tweet.user.clone());
            }
            FetchTweetResult::NotFound => {}
            FetchTweetResult::Err(e) => panic!("tweet: {tweet:?} {e:?}"),
            FetchTweetResult::RateLimit => {
                retry_tweets.push(tweet);
            }
        }
    };

    // For each discord message in the channel, do first pass of twitter scrape
    let messages = MessagesIter::<Http>::stream(&ctx, channel);
    tokio::pin!(messages);
    while let Some(message) = messages.next().await {
        let message = message.unwrap();

        // Stop if reached already processed message
        if let Some(after) = after {
            if message.id <= after {
                break;
            }
        }

        // Extract tweets
        let tweets = extract_tweets(&message.content);

        // Fetch tweet info and print for every tweet
        for tweet in tweets {
            eprintln!("read_channel_history() channel: {channel_text} found tweet: {tweet:?}");
            let fetch_tweet_result = fetch_tweet_info(&tweet).await;
            do_fetch_tweet(tweet, fetch_tweet_result, &mut retry_tweets);
        }

        last_processed_message = Some(
            last_processed_message
                .map(|m: MessageId| std::cmp::max(m, message.id))
                .unwrap_or(message.id),
        );
    }

    // Retry scraping any tweets that were rate limited after a delay
    let mut attempt_count = 0;
    let total_attempt_count = 3;
    while !retry_tweets.is_empty() && attempt_count < total_attempt_count {
        attempt_count += 1;

        const SLEEP_MIN: u64 = 10;
        eprintln!(
            "read_channel_history() channel: {} retry (attempt {}/{}) fetch {} tweets after {}m sleep",
            channel_text,
            attempt_count,
            total_attempt_count,
            retry_tweets.len(),
            SLEEP_MIN,
        );
        tokio::time::sleep(Duration::from_secs(SLEEP_MIN * 60)).await;

        let tmp = retry_tweets.clone();
        retry_tweets.clear();

        for tweet in tmp {
            let fetch_tweet_result = fetch_tweet_info(&tweet).await;
            do_fetch_tweet(tweet, fetch_tweet_result, &mut retry_tweets);
        }
    }

    eprintln!("read_channel_history() finished channel: {channel_text}");
    let channel_row = ChannelRow {
        channel,
        last_message: last_processed_message.unwrap_or(after.unwrap_or_default()),
    };
    (twitter_users, channel_row)
}

#[allow(clippy::large_enum_variant)]
enum FetchTweetResult {
    Ok(twitter_syndication::tweet::Tweet),
    NotFound,
    RateLimit,
    Err(twitter_syndication::TweetFetcherError),
}

async fn fetch_tweet_info(tweet: &Tweet) -> FetchTweetResult {
    static TWEET_FETCHER: Lazy<TweetFetcher> = Lazy::new(|| TweetFetcher::new().unwrap());
    match TWEET_FETCHER.fetch(tweet.id).await {
        Ok(SyndicationTweetType::Tweet(t)) => FetchTweetResult::Ok(t),
        Ok(SyndicationTweetType::TweetTombstone) => FetchTweetResult::NotFound,
        Err(e) => {
            if let Some(status) = e.status() {
                match status.as_u16() {
                    404 => {
                        // Most likely a rate limit
                        eprintln!(
                            "fetch tweet {} error code {} (likely rate limit)",
                            tweet.id,
                            status.as_u16(),
                        );
                        return FetchTweetResult::RateLimit;
                    }
                    400 => {
                        // Broken tweet
                        eprintln!(
                            "fetch tweet {} error code {} (likely broken tweet)",
                            tweet.id,
                            status.as_u16(),
                        );
                        return FetchTweetResult::NotFound;
                    }
                    500..600 => {
                        // Some kind of server error
                        eprintln!(
                            "error: tweet id {} status code: {} (server error)",
                            tweet.id,
                            status.as_u16()
                        );
                        return FetchTweetResult::NotFound;
                    }
                    _ => {}
                }
            }
            // This is a real unhandled error
            FetchTweetResult::Err(e)
        }
    }
}

#[derive(Ord, PartialOrd, Eq, PartialEq, Clone, Debug)]
struct Tweet {
    user: String,
    id: u64,
}

fn extract_tweets(content: &str) -> BTreeSet<Tweet> {
    static RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"\b(twitter|x)\.com/(?P<user>.*?)/status/(?P<id>\d+)").unwrap());
    RE.captures(content)
        .into_iter()
        .filter_map(|cap| {
            let user = cap.name("user").unwrap().as_str().to_lowercase();
            let id_str = cap.name("id").unwrap().as_str();
            let id = match id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    eprintln!(
                        "Found invalid tweet link: {} error: {}",
                        cap.get(0).unwrap().as_str(),
                        e,
                    );
                    return None;
                }
            };
            Some(Tweet { user, id })
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
