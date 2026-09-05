// @generated automatically by Diesel CLI.

diesel::table! {
    characters (id) {
        id -> Uuid,
        user_id -> Uuid,
        character -> Jsonb,
        data -> Jsonb,
        inventory -> Jsonb,
        wallet -> Jsonb,
        town -> Nullable<Jsonb>,
        server_state -> Jsonb,
    }
}

diesel::table! {
    quests (id, character_id) {
        id -> Uuid,
        character_id -> Uuid,
        info -> Jsonb,
        generated_data -> Jsonb,
        dungeon_state -> Nullable<Jsonb>,
        initial_state -> Nullable<Jsonb>,
    }
}

diesel::table! {
    users (id) {
        id -> Uuid,
        secret_id -> Uuid,
        data -> Jsonb,
    }
}

diesel::table! {
    guilds (id) {
        id -> Text,
        name -> Text,
        tag_id -> Text,
        guild_type -> Text,
        short_description -> Text,
        long_description -> Text,
        badge_icon_index -> Int4,
        region_index -> Int4,
        trophies -> Int8,
        created_at -> Int8,
        exchange_donation_count -> Int8,
        grandmaster_since -> Int8,
    }
}

diesel::table! {
    guild_members (guild_id, user_id) {
        guild_id -> Text,
        user_id -> Uuid,
        character_id -> Uuid,
        rank -> Text,
        join_date -> Int8,
    }
}

diesel::table! {
    guild_messages (message_id) {
        message_id -> Text,
        guild_id -> Text,
        user_id -> Uuid,
        character_id -> Uuid,
        message_type -> Text,
        type_specific_data -> Jsonb,
        creation_time -> Int8,
    }
}

diesel::table! {
    guild_exchanges (id) {
        id -> Text,
        guild_id -> Text,
        requester_user_id -> Uuid,
        requester_character_id -> Uuid,
        item_template_id -> Uuid,
        requested_amount -> Int8,
        max_donation_amount -> Int8,
        donations -> Jsonb,
        donation_sum -> Int8,
        creation_time -> Int8,
        redeemed -> Bool,
    }
}

diesel::table! {
    guild_applications (guild_id, user_id) {
        guild_id -> Text,
        user_id -> Uuid,
        character_id -> Uuid,
        state -> Text,
        creation_time -> Int8,
    }
}

diesel::table! {
    guild_removals (guild_id, user_id) {
        guild_id -> Text,
        user_id -> Uuid,
        removed_at -> Int8,
        banned -> Bool,
    }
}

diesel::joinable!(characters -> users (user_id));
diesel::joinable!(quests -> characters (character_id));

diesel::allow_tables_to_appear_in_same_query!(
    characters,
    quests,
    users,
    guilds,
    guild_members,
    guild_messages,
    guild_exchanges,
    guild_applications,
    guild_removals,
);

diesel::table! {
    arena_credentials (username) {
        username -> Text,
        user_id -> Uuid,
        password_hash -> Text,
        created_at -> Int8,
        updated_at -> Int8,
    }
}

diesel::table! {
    arena_seasons (id) {
        id -> Uuid,
        number -> Int4,
        name -> Text,
        starts_at -> Int8,
        ends_at -> Int8,
        status -> Text,
        scoring -> Text,
        reset_rule -> Text,
        created_at -> Int8,
        ended_at -> Nullable<Int8>,
    }
}

diesel::table! {
    arena_season_standings (season_id, character_id) {
        season_id -> Uuid,
        character_id -> Uuid,
        rank -> Int4,
        trophies -> Int8,
        matches -> Int4,
        wins -> Int4,
        guild_id -> Nullable<Text>,
        recorded_at -> Int8,
    }
}

diesel::table! {
    arena_season_guild_standings (season_id, guild_id) {
        season_id -> Uuid,
        guild_id -> Text,
        rank -> Int4,
        trophies -> Int8,
        members -> Int4,
        recorded_at -> Int8,
    }
}

diesel::table! {
    arena_season_awards (id) {
        id -> Uuid,
        season_id -> Uuid,
        character_id -> Uuid,
        kind -> Text,
        rank -> Int4,
        tier -> Text,
        payload -> Jsonb,
        granted_at -> Nullable<Int8>,
        created_at -> Int8,
    }
}
