# Stax

A small Plex/Jellyfin-shaped media server experiment with a Rust backend and a
React frontend. It indexes local media, serves browser-playable streams,
generates thumbnails, and has watch-together rooms with synchronized
playback over WebSockets.

This project is 100% LLM slop. Do not run it on the public internet, trust it
with anything important, or mistake it for production software. It's not. I do not give a shit.

Release builds embed the frontend into the backend binary:

```bash
cd frontend
npm install
npm run build

cd ../backend
cargo build --release
```

Example backend run:

```bash
./target/release/stax-backend \
  --api-addr 0.0.0.0:3001 \
  --library-root /path/to/media \
  --database-path ./stax.db
```
