# Syncplay Product Direction

This document defines the intended product shape for Syncplay.

The project should feel like a simple, self-hosted media server in the same general category as Plex or Jellyfin, with synchronized playback as an important feature, not the entire identity of the app.

## Core Position

Syncplay is not trying to match Plex feature-for-feature.

It is trying to match the basic product shape:

- browse a media library
- open a title
- press play immediately
- optionally start or join a shared watch session

If the app feels like an operator console, debug dashboard, or room-control surface first, it is drifting away from the target.

## What "Plex-like" Means Here

For this repo, "Plex-like" means:

- library-first navigation
- media detail pages and a normal watch flow
- a player that is the primary experience, not a side tool
- metadata, artwork, subtitles, and direct play presented as core features
- watch-together layered on top of normal playback

It does not mean:

- cloning Plex's full feature set
- prioritizing TV apps before the web client is excellent
- building a room dashboard as the main landing page
- exposing sync diagnostics as the default user experience

## Product Principles

### 1. Library First

The main surface should be the media library, not the room list.

Users should enter the app expecting to:

- browse recently added or organized media
- filter into a library section
- choose something to watch

Rooms should be reachable from that experience, not the other way around.

### 2. Playback First

The default path after choosing media should be immediate playback.

The player should feel like a normal media app player:

- title context
- subtitle selection
- obvious controls
- minimal friction between discovery and watching

### 3. Sync As A Layer

Synchronized playback is a major differentiator, but it should behave like a layer on top of normal watching.

The mental model should be:

1. pick media
2. start watching
3. optionally invite others or join a watch session

Not:

1. create a room
2. open a control surface
3. manually coordinate media around the room system

### 4. Details Over Diagnostics

Normal users should mostly see:

- titles
- metadata
- subtitles
- play state
- watch-together controls

They should not primarily see:

- drift metrics
- correction labels
- control-booth wording
- transport details

Sync diagnostics are still useful, but they belong in debug or admin views.

### 5. Media Owns The Experience

Media selection should be attached to the title and player flow.

Room state should reference concrete media, not rely on a local preview choice or placeholder room title.

That means:

- a room should point at an indexed media item
- clients joining a room should load that same media item automatically
- playback commands should operate on the room's selected media

## What Is Wrong With The Current Shape

The current prototype contains valuable backend work, but the frontend/product framing is off.

Useful foundations already exist:

- library indexing
- persistence
- stream endpoints
- subtitle handling
- room clocks and drift correction

What currently points in the wrong direction:

- room-first landing experience
- "control booth" framing
- diagnostics-heavy UI hierarchy
- player treated like a preview deck instead of the main watch surface
- media selection not yet owned by room state

## UX Direction

The intended top-level information architecture should look more like:

- Home
- Library
- Title details
- Player
- Watch Together

Possible room and sync controls:

- "Watch together" button on a title or player
- join dialog from a shared link or invite code
- lightweight participant and sync status inside the player

Possible debug/admin surfaces:

- sync diagnostics
- drift logs
- connection state detail
- scan failures and media ingestion warnings

Those should not dominate the main user journey.

## Implementation Implications

Near-term implementation should follow these rules:

1. Prefer library and playback improvements over more operator-console UI.
2. Treat the current room/control surface as transitional, not the target homepage.
3. Move sync/debug-heavy controls behind a secondary route, drawer, or debug panel.
4. Make room-selected media part of the core data model before expanding sync UX further.
5. Build title and player flows that still make sense when the user is watching alone.

## Immediate Next Product Steps

The next frontend/backend slices should move in this order:

1. make media selection room-owned instead of local-only
2. redesign the main frontend shell around library browsing and title playback
3. make the player page the default consumption surface
4. demote current sync diagnostics into debug-only UI
5. add watch-together entry points from title and player pages

## Guardrail

When choosing between two designs, prefer the one that makes Syncplay feel more like "open media server, pick a title, press play" and less like "connect to a synchronized playback lab."
