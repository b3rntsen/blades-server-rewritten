// @generated automatically by Diesel CLI.

diesel::table! {
    arena_match_results (id) {
        id -> Uuid,
        character_id -> Uuid,
        opponent_character_id -> Nullable<Uuid>,
        game_session_id -> Nullable<Uuid>,
        win -> Bool,
        rounds_won -> Int4,
        rounds_lost -> Int4,
        gold -> Int8,
        character_xp -> Int8,
        trophy_delta -> Int8,
        trophies_after -> Int8,
        matchmaking_trophies_after -> Int8,
        arena -> Int4,
        arena_level -> Int4,
        chest_meter -> Int8,
        recorded_at -> Timestamptz,
    }
}

diesel::table! {
    arena_matches (ticket_id) {
        ticket_id -> Uuid,
        user_id -> Uuid,
        status -> Text,
        game_session_id -> Nullable<Uuid>,
        paired -> Bool,
        recorded_at -> Timestamptz,
        resolved_at -> Nullable<Timestamptz>,
    }
}

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
    device_bindings (device_id) {
        device_id -> Text,
        user_id -> Nullable<Uuid>,
        platform -> Nullable<Text>,
        last_seen -> Timestamptz,
        bound_at -> Nullable<Timestamptz>,
        source_wg_ip -> Nullable<Text>,
    }
}

diesel::table! {
    event_completions (id) {
        id -> Uuid,
        character_id -> Uuid,
        event_id -> Uuid,
        completion_count -> Int4,
        last_completed_at -> Timestamp,
        created_at -> Timestamp,
    }
}

diesel::table! {
    event_dungeons (id) {
        id -> Uuid,
        character_id -> Uuid,
        event_id -> Uuid,
        dungeon_id -> Uuid,
        dungeon_state -> Nullable<Jsonb>,
        initial_state -> Nullable<Jsonb>,
        generated_data -> Jsonb,
        entered_at -> Timestamp,
        expires_at -> Nullable<Timestamp>,
        entry_count -> Int4,
        max_entries -> Int4,
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
    guild_removals (guild_id, user_id) {
        guild_id -> Text,
        user_id -> Uuid,
        removed_at -> Int8,
        banned -> Bool,
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
    matchmaking (id) {
        id -> Uuid,
        other_id -> Nullable<Uuid>,
        match_info -> Nullable<Jsonb>,
        ack_info -> Nullable<Jsonb>,
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
    sessions (session_id) {
        session_id -> Uuid,
        user_id -> Uuid,
        secret_user_id -> Uuid,
        extra_secret -> Uuid,
        expires_at -> Timestamptz,
    }
}

diesel::table! {
    users (id) {
        id -> Uuid,
        secret_id -> Uuid,
        data -> Jsonb,
    }
}

diesel::joinable!(arena_match_results -> characters (character_id));
diesel::joinable!(arena_matches -> users (user_id));
diesel::joinable!(characters -> users (user_id));
diesel::joinable!(device_bindings -> users (user_id));
diesel::joinable!(event_completions -> characters (character_id));
diesel::joinable!(event_dungeons -> characters (character_id));
diesel::joinable!(quests -> characters (character_id));

diesel::allow_tables_to_appear_in_same_query!(
    arena_match_results,
    arena_matches,
    characters,
    device_bindings,
    event_completions,
    event_dungeons,
    guild_applications,
    guild_exchanges,
    guild_members,
    guild_messages,
    guild_removals,
    guilds,
    matchmaking,
    quests,
    sessions,
    users,
);
