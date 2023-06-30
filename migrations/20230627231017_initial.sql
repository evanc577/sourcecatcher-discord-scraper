-- Add migration script here
CREATE TABLE channels (
    channel TEXT NOT NULL PRIMARY KEY,
    last_message TEXT NOT NULL
);
CREATE TABLE twitter_users (
    user TEXT NOT NULL PRIMARY KEY
);
